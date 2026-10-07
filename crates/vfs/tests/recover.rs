//! Volume recovery-image capture (§4.8, A-9): a scratch volume's whole durable state — its tree,
//! every file's bytes (inline and chunked alike), symlink targets, hard links and roots — is
//! captured into a canonical image, that image round-trips through its content bytes unchanged and
//! byte-identically, and a corrupt or foreign image is refused with a typed error rather than
//! read as a smaller volume. This is the capture half of the write→kill→restart→read proof: what a
//! running daemon would publish into anchor-owned RAM. Driven against an in-memory scratch volume
//! on every host.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod common;

use common::{store, volume};
use slates_mem::{SharedObject, Words};

/// A whole shared object as image memory, reached by its copies (AUD-29-09: the object hands out no
/// reference to its bytes). The object declares no words: the image is its only content.
struct ObjectImage(SharedObject);

impl slates_vfs::recover::ImageRead for ObjectImage {
  fn image_len(&self) -> usize {
    self.0.len()
  }

  fn image_read(&self, offset: usize, into: &mut [u8]) -> Result<(), slates_vfs::VfsError> {
    self
      .0
      .read(offset, into)
      .map_err(|_| slates_vfs::VfsError::RecoveryIncomplete)
  }
}

impl slates_vfs::recover::ImageWrite for ObjectImage {
  fn image_write(&mut self, offset: usize, from: &[u8]) -> Result<(), slates_vfs::VfsError> {
    self
      .0
      .write(offset, from)
      .map_err(|_| slates_vfs::VfsError::NoSpace)
  }
}
use slates_vfs::VfsError;
use slates_vfs::clock::StepClock;
use slates_vfs::ids::InodeNo;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::{BudgetGrowth, Quota};
use slates_vfs::recover::{
  BodyImage, EntryImage, ImageOut, InodeImage, KeyedImage, KindImage, PolicyImage, ShardImage,
  SharedSource, VolumeImage,
};
use slates_vfs::volume::{Store, Volume, VolumeConfig};

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

/// A one-file volume's image, the file holding `content`. Two calls with different content give two
/// distinct images, for the atomic-publication tests.
fn image_with(content: &[u8]) -> VolumeImage {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, f, 0, content).unwrap();
  vol.to_image(&store, None).unwrap()
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
  let BodyImage::Directory { entries, .. } = &inode(image, dir).body else {
    panic!("inode {dir:?} is a directory in the image");
  };
  entries
    .iter()
    .find(|e| e.name == name)
    .expect("the entry is present")
    .child
    .expect("a visible inode")
}

/// A scratch volume with a small inline file, a multi-chunk file in a subdirectory, a symlink and a
/// hard link, together with its captured image and the ids the assertions check.
struct Built {
  store: Store,
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
  let image = vol.to_image(&store, None).unwrap();
  Built {
    store,
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
    BodyImage::Inline {
      bytes: b"hello".to_vec()
    }
  );
}

/// AC (§4.8, A-64): a multi-chunk file is imaged by reference. Do: capture a volume holding a 256 KiB file,
/// then rebuild it over the surviving arena. Expect: the image's bytes are a small fraction of the file's (it
/// names the chunks; it does not carry them), and the rebuilt file reads back every byte, whole and in order.
#[test]
fn a_multi_chunk_file_is_imaged_by_reference_and_reads_back_whole() {
  let b = built();
  let big = inode(&b.image, b.big);
  assert_eq!(big.kind, KindImage::File);
  assert!(
    matches!(big.body, BodyImage::Chunked { .. }),
    "{:?}",
    big.body
  );
  let content = b.image.to_content();
  assert!(
    content.len() < b.big_bytes.len() / 16,
    "the image names the chunks, not their bytes: {} bytes for a {}-byte file",
    content.len(),
    b.big_bytes.len()
  );
  let mut fresh = common::surviving(&b.store);
  let claims = common::claims(&mut fresh, &[&b.image]);
  let vol = Volume::from_image(
    &mut fresh,
    &b.image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  let mut got = vec![0u8; b.big_bytes.len()];
  let mut read = 0;
  while read < got.len() {
    let n = vol
      .read(
        &fresh,
        b.big,
        u64::try_from(read).unwrap(),
        &mut got[read..],
      )
      .unwrap();
    assert!(n > 0, "the rebuilt file ends early at {read}");
    read += n;
  }
  assert!(got == b.big_bytes, "every byte back, whole and in order");
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
  let mut fresh = common::surviving(&b.store);
  let image = VolumeImage::from_content(&original).unwrap();
  let claims = common::claims(&mut fresh, &[&image]);
  let vol = Volume::from_image(
    &mut fresh,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();

  // The rebuilt volume re-images byte-for-byte identically: nothing was lost or changed.
  let rebuilt = vol.to_image(&fresh, None).unwrap().to_content();
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
  let mut object =
    ObjectImage(SharedObject::create(&object_name("h"), CONTENT_LEN, Words::new()).unwrap());
  let handoff = object.0.handoff().unwrap();
  b.image.write_to(&mut object).unwrap();

  // The restarted daemon re-opens the same object; the writer's mapping then goes away.
  let reattached = ObjectImage(SharedObject::open(&handoff, CONTENT_LEN, Words::new()).unwrap());
  drop(object);
  let image = VolumeImage::read_from(&reattached)
    .unwrap()
    .expect("the published image is present after the handoff");

  // Rebuild and read the bytes written before the "restart" through their original inode number.
  let mut fresh = common::surviving(&b.store);
  let claims = common::claims(&mut fresh, &[&image]);
  let vol = Volume::from_image(
    &mut fresh,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  let mut got = vec![0u8; b.big_bytes.len()];
  let n = vol.read(&fresh, b.big, 0, &mut got).unwrap();
  got.truncate(n);
  assert_eq!(
    got, b.big_bytes,
    "bytes written before the content-object handoff read back after it"
  );
  assert_eq!(
    vol.to_image(&fresh, None).unwrap(),
    image,
    "the rebuilt volume is fully faithful"
  );
}

/// AC (§4.2 resource vector): the inode allowance bounds a volume's live inodes independently of its
/// byte quota — empty files are refused with `NoSpace` at the allowance even though byte space
/// remains — and an unlink returns the credit so a create succeeds again.
#[test]
fn the_inode_allowance_bounds_empty_files_and_frees_on_unlink() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30); // a huge byte quota
  vol.set_inode_allowance(3).unwrap(); // but only three live inodes: the root and two more
  let root = vol.root_inode(&store).unwrap();
  assert_eq!(vol.inode_usage(), (1, 3), "the root is the one live inode");

  vol.create_file_no(&mut store, root, "a", 0o644).unwrap();
  vol.create_file_no(&mut store, root, "b", 0o644).unwrap();
  assert_eq!(vol.inode_usage(), (3, 3), "at the allowance");
  assert!(
    matches!(
      vol.create_file_no(&mut store, root, "c", 0o644),
      Err(VfsError::NoSpace)
    ),
    "an empty file beyond the inode allowance is refused, though byte space remains"
  );

  vol.unlink_no(&mut store, root, "a").unwrap();
  assert_eq!(
    vol.inode_usage(),
    (2, 3),
    "the unlink returned an inode credit"
  );
  vol
    .create_file_no(&mut store, root, "c", 0o644)
    .expect("a create succeeds again after the unlink freed a credit");
}

/// AC (§4.2 namespace dimension): the entry allowance bounds a volume's live directory entries —
/// including hard links, which add a name without an inode, so neither the byte quota nor the inode
/// allowance bounds them — and an unlink returns the credit.
#[test]
fn the_entry_allowance_bounds_hard_links_and_frees_on_unlink() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  vol.set_entry_allowance(3).unwrap(); // the root starts empty; three names allowed
  let f = vol.create_file_no(&mut store, root, "a", 0o644).unwrap();
  vol.link_no(&mut store, root, "b", f).unwrap(); // a hard link: same inode, a new entry
  vol.link_no(&mut store, root, "c", f).unwrap();
  assert_eq!(vol.entry_usage(), (3, 3), "at the entry allowance");
  assert!(
    matches!(
      vol.link_no(&mut store, root, "d", f),
      Err(VfsError::NoSpace)
    ),
    "a fourth name is refused though it is one inode and byte space remains"
  );

  vol.unlink_no(&mut store, root, "a").unwrap();
  assert_eq!(
    vol.entry_usage(),
    (2, 3),
    "the unlink returned an entry credit"
  );
  vol
    .link_no(&mut store, root, "d", f)
    .expect("a link succeeds again after the unlink freed a credit");
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

  let image = clone.to_image(&src, None).unwrap();
  assert!(
    image.origin_epoch.is_some(),
    "a clone records its origin epoch"
  );

  // A clone shares its origin snapshot's records (A-64), so it recovers beside its recovered origin.
  let origin_image = origin.to_image(&src, None).unwrap();
  let mut fresh = common::surviving(&src);
  let claims = common::claims(&mut fresh, &[&origin_image, &image]);
  let recovered_origin = Volume::from_image(
    &mut fresh,
    &origin_image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  let recovered = Volume::clone_from_image(
    &mut fresh,
    &image,
    &claims,
    &recovered_origin,
    snap,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  assert_eq!(
    recovered.to_image(&fresh, None).unwrap(),
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
      image: a.to_image(&original, None).unwrap(),
    },
    KeyedImage {
      key: key_bytes(8),
      image: b.to_image(&original, None).unwrap(),
    },
  ]);
  let mut object =
    ObjectImage(SharedObject::create(&object_name("s"), CONTENT_LEN, Words::new()).unwrap());
  let handoff = object.0.handoff().unwrap();
  shard.write_to(&mut object).unwrap();

  // The restarted daemon re-opens the object and recovers every volume into a fresh store.
  let reattached = ObjectImage(SharedObject::open(&handoff, CONTENT_LEN, Words::new()).unwrap());
  drop(object);
  let recovered = ShardImage::read_from(&reattached)
    .unwrap()
    .expect("the shard image is present after the handoff");
  let keys: Vec<[u8; 16]> = recovered.volumes.iter().map(|v| v.key).collect();
  assert_eq!(
    keys,
    vec![key_bytes(7), key_bytes(8)],
    "both volumes recovered, in key order"
  );

  let mut fresh = common::surviving(&original);
  let images: Vec<&VolumeImage> = recovered.volumes.iter().map(|keyed| &keyed.image).collect();
  let claims = common::claims(&mut fresh, &images);
  for keyed in &recovered.volumes {
    let vol = Volume::from_image(
      &mut fresh,
      &keyed.image,
      &claims,
      Box::new(StepClock::new(0, 1)),
      1 << 16,
      None,
    )
    .unwrap();
    assert_eq!(
      vol.to_image(&fresh, None).unwrap(),
      keyed.image,
      "volume with key {:?} rebuilds faithfully",
      keyed.key
    );
  }
}

/// AC (§4.8): the production shard-publication seam is atomic. `ShardImage::write_to` and
/// `read_from` are exactly what the daemon's `publish_shard`/`recover_images` call, so this drives
/// them: a shard image commits, a second shard image's publish is torn mid-write, and recovery reads
/// back the *first* — the last committed shard, not a torn or empty one. This is the memory-level
/// shape of a daemon killed in the middle of publishing to anchor-owned RAM.
#[test]
fn an_interrupted_shard_publish_preserves_the_last_committed_shard() {
  let one = ShardImage::new(vec![KeyedImage {
    key: key_bytes(7),
    image: image_with(b"committed-shard"),
  }]);
  let two = ShardImage::new(vec![KeyedImage {
    key: key_bytes(7),
    image: image_with(b"in-flight-shard"),
  }]);
  let mut buf = vec![0u8; CONTENT_LEN];

  one.write_to(&mut buf).unwrap();
  assert_eq!(
    ShardImage::read_from(&buf).unwrap().as_ref(),
    Some(&one),
    "the first shard image commits"
  );

  let slot_len = two.write_to(&mut buf).unwrap();
  let half = buf.len() / 2;
  buf[half + slot_len - 1] ^= 0xFF; // the second shard landed in the inactive slot; tear it
  assert_eq!(
    ShardImage::read_from(&buf).unwrap().as_ref(),
    Some(&one),
    "a torn shard publish preserves the last committed shard image at the production seam"
  );
}

/// AC (§4.8): a dynamic volume recovers — its `Dynamic` quota (max, granted, denied) and the growth
/// it took are restored, not refused. The growth source is the stateless `BudgetGrowth`, so only the
/// counters travel in the image; the volume round-trips and holds the same granted growth, and its
/// grown content reads back. (Before dynamic growth was admitted through the budget, a dynamic quota
/// could not be serialized and recovery refused it with `RecoveryIncomplete`.)
#[test]
fn a_dynamic_volume_recovers_with_its_quota_and_growth() {
  let mut src = store();
  let mut vol = Volume::create(
    &mut src,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Fold,
      quota: Quota::Dynamic {
        max: 1 << 40,
        source: Box::new(BudgetGrowth),
        granted: 0,
        denied: 0,
      },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(0, 1)),
    },
  )
  .unwrap();
  let root = vol.root_inode(&src).unwrap();
  let f = vol.create_file_no(&mut src, root, "f", 0o644).unwrap();
  vol.write(&mut src, f, 0, &vec![7u8; 128 * 1024]).unwrap(); // grows the dynamic quota from the budget
  let grown = vol.budget_hold();
  assert!(grown > 0, "the dynamic volume took growth from the budget");

  let image = vol.to_image(&src, None).unwrap();
  let mut fresh = common::surviving(&src);
  let claims = common::claims(&mut fresh, &[&image]);
  let recovered = Volume::from_image(
    &mut fresh,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  assert_eq!(
    recovered.to_image(&fresh, None).unwrap(),
    image,
    "the dynamic volume round-trips: max, granted and denied are preserved"
  );
  assert_eq!(
    recovered.budget_hold(),
    grown,
    "the granted growth is restored, not lost"
  );
  let mut buf = vec![0u8; 128 * 1024];
  let n = recovered.read(&fresh, f, 0, &mut buf).unwrap();
  assert_eq!(n, 128 * 1024, "the grown content reads back after recovery");
}

/// AC (§4.2 retention): `retained_versions` counts the inode versions a volume's snapshots pin after
/// the head diverges — the version-slab pressure snapshots add beyond the live inodes. Computed from
/// the deadlists, it is zero for a fresh snapshot (which shares the head), rises by one for each file
/// that diverges from the snapshot (an unchanged file pins none), and returns to zero when the
/// snapshot is destroyed and its pinned versions are released. This is the accounting the §4.2
/// retention charge (docs/wip/resource-vector.md §3) will bound; the count itself is verified here.
#[test]
fn retained_versions_counts_the_snapshot_pinned_inode_versions() {
  let mut src = store();
  let mut vol = volume(&mut src, 1 << 30);
  let root = vol.root_inode(&src).unwrap();
  let a = vol.create_file_no(&mut src, root, "a", 0o644).unwrap();
  let b = vol.create_file_no(&mut src, root, "b", 0o644).unwrap();
  let c = vol.create_file_no(&mut src, root, "c", 0o644).unwrap();
  vol.write(&mut src, a, 0, b"a1").unwrap();
  vol.write(&mut src, b, 0, b"b1").unwrap();
  vol.write(&mut src, c, 0, b"c1").unwrap();
  let snap = vol.snapshot(&mut src).unwrap();
  assert_eq!(
    vol.retained_versions(),
    0,
    "a fresh snapshot pins nothing — it shares the head's inodes"
  );
  vol.write(&mut src, a, 0, b"a2").unwrap();
  assert_eq!(
    vol.retained_versions(),
    1,
    "diverging a pins its frozen version"
  );
  vol.write(&mut src, b, 0, b"b2").unwrap();
  assert_eq!(
    vol.retained_versions(),
    2,
    "diverging b pins another; c (unchanged) pins none"
  );
  vol.destroy_snapshot(&mut src, snap).unwrap();
  assert_eq!(
    vol.retained_versions(),
    0,
    "destroying the snapshot releases every version it pinned"
  );
}

/// AC (§4.2 retention charge): a diverging write that would pin an inode version past the retention
/// allowance is refused with `NoSpace` before the copy-up — so one volume's snapshots cannot consume
/// the shared version slab past their bound — and the refused write changes nothing. Below the
/// allowance a diverging write succeeds; destroying the snapshot releases the pinned versions. The
/// allowance is unbounded by default, so only a volume an owner has bounded is affected.
#[test]
fn the_retention_allowance_refuses_a_diverging_write_past_the_bound() {
  let mut src = store();
  let mut vol = volume(&mut src, 1 << 30);
  vol.set_retention_allowance(1).unwrap(); // at most one pinned version
  let root = vol.root_inode(&src).unwrap();
  let a = vol.create_file_no(&mut src, root, "a", 0o644).unwrap();
  let b = vol.create_file_no(&mut src, root, "b", 0o644).unwrap();
  vol.write(&mut src, a, 0, b"a1").unwrap();
  vol.write(&mut src, b, 0, b"b1").unwrap();
  let snap = vol.snapshot(&mut src).unwrap();

  // Diverging `a` pins one version — at the allowance.
  vol.write(&mut src, a, 0, b"a2").unwrap();
  assert_eq!(vol.retained_versions(), 1, "a's frozen version is pinned");

  // Diverging `b` would pin a second — refused, and nothing changes.
  assert!(
    matches!(vol.write(&mut src, b, 0, b"b2"), Err(VfsError::NoSpace)),
    "a diverging write past the retention allowance is refused"
  );
  assert_eq!(
    vol.retained_versions(),
    1,
    "the refused write pinned nothing"
  );
  let mut buf = vec![0u8; 8];
  let n = vol.read(&src, b, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..n],
    b"b1",
    "the refused write left b unchanged — no partial copy-up"
  );

  // Destroying the snapshot releases the pinned version.
  vol.destroy_snapshot(&mut src, snap).unwrap();
  assert_eq!(
    vol.retained_versions(),
    0,
    "destroying the snapshot releases its pinned version"
  );
}

/// AC (§4.8): a referenced-but-unlinked orphan's content survives recovery — its acknowledged bytes
/// are not lost. A file is opened (referenced), then unlinked while open (POSIX unlink-while-open):
/// its name is gone but its inode stays allocated in the table, so the image (which captures every
/// inode the table reaches, by number) captures it, and recovery rebuilds it. The recovered volume
/// serves its bytes by the same inode number a handoff-reacquired handle would use.
#[test]
fn an_unlinked_but_open_orphan_recovers_its_content() {
  let mut src = store();
  let mut vol = volume(&mut src, 1 << 30);
  let root = vol.root_inode(&src).unwrap();
  let f = vol.create_file_no(&mut src, root, "f", 0o644).unwrap();
  vol.write(&mut src, f, 0, b"orphan-bytes").unwrap();
  vol.reference(&src, f).unwrap(); // a transport holds it open
  vol.unlink_no(&mut src, root, "f").unwrap(); // unlink while open → orphan
  assert!(
    vol.lookup_no(&src, root, "f").is_err(),
    "the orphan has no name in the tree"
  );
  let mut buf = vec![0u8; 16];
  let n = vol.read(&src, f, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..n],
    b"orphan-bytes",
    "the orphan is still readable by number while open"
  );

  let image = vol.to_image(&src, None).unwrap();
  assert!(
    image.inodes.iter().any(|i| i.no == f.0),
    "the orphan inode is captured in the image (the walk covers the inode table, not just the tree)"
  );
  let mut fresh = common::surviving(&src);
  let claims = common::claims(&mut fresh, &[&image]);
  let mut recovered = Volume::from_image(
    &mut fresh,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  let n = recovered.read(&fresh, f, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..n],
    b"orphan-bytes",
    "the orphan's acknowledged content survives recovery (§4.8), not lost with its name"
  );
  // The orphan *tracking* is restored too, not just the bytes: a handle reacquired through the anchor
  // handoff (a reference) and then closed (unreference) reclaims the orphan, rather than leaking it.
  // Without restoring the orphan set this last close would not reclaim, so the assertion is non-vacuous.
  recovered.reference(&fresh, f).unwrap();
  recovered.unreference(&mut fresh, f).unwrap();
  assert!(
    recovered.read(&fresh, f, 0, &mut buf).is_err(),
    "the recovered orphan is reclaimed when its reacquired handle closes, not leaked"
  );
}

/// AC (§4.8): publication into the content object is an atomic double-buffered commit — an
/// interrupted, torn or too-large publish preserves the last committed image and never presents a
/// torn slot as a success. A fresh object holds nothing; a publish commits; a publish torn mid-write
/// leaves the previous commit intact; a retry commits; a buffer too small to hold a slot refuses the
/// publish without touching the committed slot; and an object with no valid commit reads as `None`
/// (which the caller turns into `RecoveryIncomplete`), never a garbage success.
#[test]
fn an_interrupted_publish_preserves_the_last_committed_image() {
  let first = image_with(b"first-committed-value");
  let second = image_with(b"second-in-flight-value");
  let mut buf = vec![0u8; CONTENT_LEN];

  assert!(
    VolumeImage::read_from(&buf).unwrap().is_none(),
    "a fresh content object has no committed image"
  );

  VolumeImage::write_to(&first, &mut buf).unwrap();
  assert_eq!(
    VolumeImage::read_from(&buf).unwrap().as_ref(),
    Some(&first),
    "the first image is committed and reads back"
  );

  // Publish the second image, then tear its slot: the write reached the content object but the
  // process died before the frame was coherent (its CRC no longer matches). The commit is the CRC
  // becoming valid, so a torn slot was never a commit.
  let slot_len = VolumeImage::write_to(&second, &mut buf).unwrap();
  let half = buf.len() / 2;
  buf[half + slot_len - 1] ^= 0xFF; // the second image landed in the inactive (second) slot
  assert_eq!(
    VolumeImage::read_from(&buf).unwrap().as_ref(),
    Some(&first),
    "an interrupted publish preserves the last committed image, not a torn or empty one"
  );

  // A retry after the interruption commits the second image cleanly.
  VolumeImage::write_to(&second, &mut buf).unwrap();
  assert_eq!(
    VolumeImage::read_from(&buf).unwrap().as_ref(),
    Some(&second),
    "a retry after the interrupted publish commits"
  );

  // Both slots torn: no valid commit — `None`, which the caller refuses, never a false success.
  for byte in buf.iter_mut() {
    *byte ^= 0xFF;
  }
  assert!(
    VolumeImage::read_from(&buf).unwrap().is_none(),
    "an object with no valid commit yields None (the caller then refuses), never garbage"
  );

  let mut tiny = vec![0u8; 8];
  assert!(
    matches!(first.write_to(&mut tiny), Err(VfsError::NoSpace)),
    "a buffer too small for a slot refuses the publish"
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

  let image = vol.to_image(&store, None).unwrap();
  assert_eq!(image.snapshots.len(), 1, "the snapshot is captured");

  let head = image.inodes.iter().find(|i| i.no == f.0).unwrap();
  assert_eq!(
    head.body,
    BodyImage::Inline {
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
    BodyImage::Inline {
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

  let original = vol.to_image(&src, None).unwrap();

  let mut fresh = common::surviving(&src);
  let claims = common::claims(&mut fresh, &[&original]);
  let recovered = Volume::from_image(
    &mut fresh,
    &original,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();

  assert_eq!(
    recovered.to_image(&fresh, None).unwrap(),
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

/// AC (§4.8): a snapshot id survives recovery even when it is not the first slot — the `SnapshotRef`
/// guarantee that "a snapshot id a client holds still resolves after recovery" must hold for a
/// volume with more than one snapshot and for the survivors of a destroy, not only the trivial
/// single-snapshot case. Two snapshots are taken, the first destroyed, and the survivor (whose id is
/// slot one, not slot zero) must still serve its frozen content through the very id the client holds.
#[test]
fn a_recovered_snapshot_id_survives_when_it_is_not_the_first_slot() {
  let mut src = store();
  let mut vol = volume(&mut src, 1 << 30);
  let root = vol.root_inode(&src).unwrap();
  let f = vol.create_file_no(&mut src, root, "f", 0o644).unwrap();
  vol.write(&mut src, f, 0, b"first").unwrap();
  let snap_a = vol.snapshot(&mut src).unwrap();
  vol.write(&mut src, f, 0, b"second").unwrap();
  let snap_b = vol.snapshot(&mut src).unwrap();
  vol.write(&mut src, f, 0, b"head").unwrap();
  vol.destroy_snapshot(&mut src, snap_a).unwrap(); // frees slot zero; snap_b keeps slot one

  let image = vol.to_image(&src, None).unwrap();
  let mut fresh = common::surviving(&src);
  let claims = common::claims(&mut fresh, &[&image]);
  let recovered = Volume::from_image(
    &mut fresh,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();

  let mut buf = vec![0u8; 16];
  let n = recovered.read_in(&fresh, snap_b, f, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..n],
    b"second",
    "the surviving snapshot serves its frozen content through the id the client still holds"
  );
}

/// AC (§4.8): dropping a recovered snapshot reclaims its tree. `destroy_snapshot` reclaims only from
/// the snapshot's deadlist, and a recovered snapshot's tree is independent, so recovery must give it
/// a deadlist of its whole tree — otherwise the snapshot's chunks leak on drop. With a chunked file
/// snapshotted then overwritten, the recovered snapshot holds its own copy of the old chunks, and
/// dropping it frees them (an empty deadlist, the bug, would free nothing).
#[test]
fn dropping_a_recovered_snapshot_frees_its_tree() {
  let mut src = store();
  let mut vol = volume(&mut src, 1 << 30);
  let root = vol.root_inode(&src).unwrap();
  let f = vol.create_file_no(&mut src, root, "f", 0o644).unwrap();
  vol
    .write(&mut src, f, 0, b"content the snapshot keeps")
    .unwrap();
  let snap = vol.snapshot(&mut src).unwrap();

  let image = vol.to_image(&src, None).unwrap();
  let mut fresh = common::surviving(&src);
  let claims = common::claims(&mut fresh, &[&image]);
  let mut recovered = Volume::from_image(
    &mut fresh,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();

  // The recovered snapshot is an independent tree (its own root and file inode), so dropping it must
  // free those inode slab slots. An empty deadlist (the bug) would free nothing.
  let before = fresh.inodes.len();
  recovered.destroy_snapshot(&mut fresh, snap).unwrap();
  let after = fresh.inodes.len();
  assert!(
    after < before,
    "dropping the recovered snapshot freed its inodes ({before} -> {after})"
  );
}

/// AC (§4.8): dropping a recovered snapshot never frees content the head still reaches. With a file
/// unchanged since the snapshot (so head and snapshot hold identical content) and another changed
/// after it, dropping the snapshot must leave both head files readable — the double-free guard for
/// any head↔snapshot sharing the rebuild introduces.
#[test]
fn dropping_a_recovered_snapshot_leaves_the_head_readable() {
  let mut src = store();
  let mut vol = volume(&mut src, 1 << 30);
  let root = vol.root_inode(&src).unwrap();
  let unchanged = vol
    .create_file_no(&mut src, root, "unchanged", 0o644)
    .unwrap();
  vol
    .write(&mut src, unchanged, 0, b"identical in head and snapshot")
    .unwrap();
  let changed = vol
    .create_file_no(&mut src, root, "changed", 0o644)
    .unwrap();
  vol.write(&mut src, changed, 0, b"v1").unwrap();
  let snap = vol.snapshot(&mut src).unwrap();
  vol.write(&mut src, changed, 0, b"version-two").unwrap(); // longer, so it fully overwrites

  let image = vol.to_image(&src, None).unwrap();
  let mut fresh = common::surviving(&src);
  let claims = common::claims(&mut fresh, &[&image]);
  let mut recovered = Volume::from_image(
    &mut fresh,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();

  recovered.destroy_snapshot(&mut fresh, snap).unwrap();

  let mut buf = vec![0u8; 40];
  let n = recovered.read(&fresh, unchanged, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..n],
    b"identical in head and snapshot",
    "the head's unchanged file survives the snapshot drop"
  );
  let n = recovered.read(&fresh, changed, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..n],
    b"version-two",
    "the head's changed file survives"
  );
}

/// AC (§4.2/§4.8): a recovered snapshot shares an unchanged file's inode with the head rather than
/// rebuilding a private copy, so the store holds it once. Head = root, a, b (3 inodes); the snapshot
/// adds its own root and its own older `b` (2), and shares `a` with the head — so five inodes, not
/// six. With sharing disabled this would be six, which makes the test non-vacuous.
#[test]
fn a_recovered_snapshot_shares_unchanged_inodes_with_the_head() {
  let mut src = store();
  let mut vol = volume(&mut src, 1 << 30);
  let root = vol.root_inode(&src).unwrap();
  let a = vol.create_file_no(&mut src, root, "a", 0o644).unwrap();
  vol.write(&mut src, a, 0, b"stable").unwrap();
  let b = vol.create_file_no(&mut src, root, "b", 0o644).unwrap();
  vol.write(&mut src, b, 0, b"v1").unwrap();
  let _snap = vol.snapshot(&mut src).unwrap();
  vol.write(&mut src, b, 0, b"v2-longer").unwrap(); // b differs; a is unchanged

  let image = vol.to_image(&src, None).unwrap();
  let mut fresh = common::surviving(&src);
  let claims = common::claims(&mut fresh, &[&image]);
  let _recovered = Volume::from_image(
    &mut fresh,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  assert_eq!(
    fresh.inodes.len(),
    5,
    "the snapshot shares the unchanged file's inode with the head (six without sharing)"
  );
}

/// AC (§4.2/§4.8): the captured image is a delta, not a full second copy. A file a snapshot shares
/// unchanged with the head is named in the snapshot's `shared` list and omitted from its `inodes`,
/// so the bytes live once in the image; a file that diverged is captured in full. This is what keeps
/// a heavily-snapshotted volume's image inside its content-object slice rather than K copies of it.
#[test]
fn a_snapshot_image_holds_a_delta_not_a_second_copy_of_the_head() {
  let mut src = store();
  let mut vol = volume(&mut src, 1 << 30);
  let root = vol.root_inode(&src).unwrap();
  let unchanged = vol.create_file_no(&mut src, root, "a", 0o644).unwrap();
  vol.write(&mut src, unchanged, 0, b"stable").unwrap();
  let diverged = vol.create_file_no(&mut src, root, "b", 0o644).unwrap();
  vol.write(&mut src, diverged, 0, b"v1").unwrap();
  let _snap = vol.snapshot(&mut src).unwrap();
  vol.write(&mut src, diverged, 0, b"v2-longer").unwrap(); // b diverges; a stays shared

  let image = vol.to_image(&src, None).unwrap();
  let snap = &image.snapshots[0];
  assert!(
    snap
      .shared
      .iter()
      .any(|s| s.number == unchanged.0 && s.source == SharedSource::Head),
    "the unchanged file is recorded as shared with the head"
  );
  assert!(
    snap.inodes.iter().all(|i| i.no != unchanged.0),
    "the unchanged file's bytes are not copied into the snapshot image"
  );
  assert!(
    snap.shared.iter().all(|s| s.number != diverged.0),
    "the diverged file is not shared"
  );
  assert!(
    snap.inodes.iter().any(|i| i.no == diverged.0),
    "the diverged file is captured in full in the snapshot image"
  );
}

/// AC (§4.2/§4.8): a version frozen identically in two snapshots but diverged from the head — edit a
/// file after a run of snapshots — is captured once, not once per snapshot, and recovered as one
/// shared inode, not one per snapshot (the live volume shares it by CoW, so a faithful recovery must
/// too). The later snapshot names the earlier as its source; the image omits the second copy; both
/// snapshots serve the version through their own ids; the store holds it once; and dropping the older
/// leaves the newer readable, because the shared inode sits on the newest snapshot's deadlist.
#[test]
fn a_version_shared_across_snapshots_is_captured_once_and_recovers_shared() {
  let mut src = store();
  let mut vol = volume(&mut src, 1 << 30);
  let root = vol.root_inode(&src).unwrap();
  let f = vol.create_file_no(&mut src, root, "f", 0o644).unwrap();
  vol.write(&mut src, f, 0, b"stable-value").unwrap();
  let snap_a = vol.snapshot(&mut src).unwrap();
  let snap_b = vol.snapshot(&mut src).unwrap(); // f unchanged between a and b
  vol.write(&mut src, f, 0, b"edited-in-head").unwrap(); // f diverges from both snapshots

  let image = vol.to_image(&src, None).unwrap();
  let a = image
    .snapshots
    .iter()
    .find(|s| s.id.index == snap_a.index)
    .unwrap();
  let b = image
    .snapshots
    .iter()
    .find(|s| s.id.index == snap_b.index)
    .unwrap();
  assert!(
    a.inodes.iter().any(|i| i.no == f.0),
    "the earlier snapshot is the canonical holder of the shared version"
  );
  assert!(
    b.inodes.iter().all(|i| i.no != f.0),
    "the later snapshot does not carry a second copy of the shared version"
  );
  assert!(
    b.shared.iter().any(|s| s.number == f.0
      && matches!(s.source, SharedSource::Snapshot { at }
        if at.index == snap_a.index && at.generation == snap_a.generation)),
    "the later snapshot names the earlier as the source of the shared version"
  );

  let mut fresh = common::surviving(&src);
  let claims = common::claims(&mut fresh, &[&image]);
  let mut recovered = Volume::from_image(
    &mut fresh,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  assert_eq!(
    recovered.to_image(&fresh, None).unwrap(),
    image,
    "the deduplicated image round-trips: the rebuilt store re-captures to the same image"
  );
  assert_eq!(
    fresh.inodes.len(),
    5,
    "the shared version is one inode across both snapshots (head root, head f, root a, shared f, \
     root b); an independent copy per snapshot would be six"
  );
  let mut buf = vec![0u8; 16];
  let n = recovered.read_in(&fresh, snap_a, f, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..n],
    b"stable-value",
    "snapshot a serves the shared version"
  );
  let n = recovered.read_in(&fresh, snap_b, f, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..n],
    b"stable-value",
    "snapshot b serves the same version"
  );

  // The shared inode is on the newest snapshot's deadlist, so dropping the older snapshot does not
  // free it; the newer still reads it. Then dropping the newer is clean.
  recovered.destroy_snapshot(&mut fresh, snap_a).unwrap();
  let n = recovered.read_in(&fresh, snap_b, f, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..n],
    b"stable-value",
    "dropping snapshot a does not free snapshot b's copy of the shared version"
  );
  recovered.destroy_snapshot(&mut fresh, snap_b).unwrap();
}

/// AC (§4.8): a version shared across a chain of snapshots recovers as one inode and survives being
/// dropped newest-first with an intervening allocation that reuses freed slots — the adversarial case
/// for the global newest-referencer-first deadlist reconstruction. If the shared inode were on more
/// than one snapshot's deadlist, dropping the newest would free it early (or, worse, a later drop
/// would free a slot a fresh file has since reused). The reconstruction puts it on exactly the newest
/// snapshot, and `destroy_snapshot` migrates it toward the oldest, so it is freed once, when its last
/// referencer goes, and never leaks.
#[test]
fn a_shared_version_survives_newest_first_drops_with_slot_reuse() {
  let mut src = store();
  let mut vol = volume(&mut src, 1 << 30);
  let root = vol.root_inode(&src).unwrap();
  let f = vol.create_file_no(&mut src, root, "f", 0o644).unwrap();
  vol.write(&mut src, f, 0, b"frozen").unwrap();
  let s1 = vol.snapshot(&mut src).unwrap();
  let s2 = vol.snapshot(&mut src).unwrap();
  let s3 = vol.snapshot(&mut src).unwrap(); // f unchanged across all three
  vol.write(&mut src, f, 0, b"head-edit").unwrap(); // f diverges; all three share the frozen version

  let image = vol.to_image(&src, None).unwrap();
  let mut fresh = common::surviving(&src);
  let claims = common::claims(&mut fresh, &[&image]);
  let mut rec = Volume::from_image(
    &mut fresh,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  assert_eq!(
    fresh.inodes.len(),
    6,
    "one shared version, not one per snapshot: head root, head f, three snapshot roots, one shared f"
  );

  let mut buf = vec![0u8; 16];
  let reads = |rec: &Volume, fresh: &_, snap, buf: &mut [u8]| {
    let n = rec.read_in(fresh, snap, f, 0, buf).unwrap();
    buf[..n].to_vec()
  };

  // Drop the newest, then allocate a fresh file that reuses the just-freed slots. If the shared
  // inode had been freed by this drop (or double-listed), the new file would land on its slot and
  // the later drops would corrupt it.
  rec.destroy_snapshot(&mut fresh, s3).unwrap();
  let g = rec.create_file_no(&mut fresh, root, "g", 0o644).unwrap();
  rec.write(&mut fresh, g, 0, b"g-bytes").unwrap();
  assert_eq!(
    reads(&rec, &fresh, s1, &mut buf),
    b"frozen",
    "s1 after s3 drop"
  );
  assert_eq!(
    reads(&rec, &fresh, s2, &mut buf),
    b"frozen",
    "s2 after s3 drop"
  );

  rec.destroy_snapshot(&mut fresh, s2).unwrap();
  assert_eq!(
    reads(&rec, &fresh, s1, &mut buf),
    b"frozen",
    "s1 after s2 drop"
  );

  rec.destroy_snapshot(&mut fresh, s1).unwrap();
  // The head and the reused-slot file are intact — no drop freed a slot they hold.
  let n = rec.read(&fresh, f, 0, &mut buf).unwrap();
  assert_eq!(&buf[..n], b"head-edit", "the head's version is untouched");
  let n = rec.read(&fresh, g, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..n],
    b"g-bytes",
    "the file that reused a freed slot is untouched"
  );
  // No leak: with every snapshot gone, only the head's reachable inodes remain (root, f, g).
  assert_eq!(
    fresh.inodes.len(),
    3,
    "the shared version was freed exactly once, when its last referencer was destroyed"
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

  let image = vol.to_image(&store, None).unwrap();
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

  let first = vol.to_image(&store, None).unwrap().to_content();
  let second = vol.to_image(&store, None).unwrap().to_content();
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
  let good = vol.to_image(&store, None).unwrap().to_content();

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

/// A hostile-input gate on the *framed, double-buffered* production path (`read_from`, what
/// `recover_images` calls on a possibly-corrupt anchor content object): a huge frame length must not
/// allocate (the length is checked against the buffer before any decode — no OOM), bit-flipped or
/// payload-corrupt content commits nothing (the CRC catches it), and none of it panics or is read as
/// a garbage success. This is the entry a corrupt anchor object actually reaches on a restart.
#[test]
fn a_hostile_framed_content_object_never_allocates_or_panics() {
  let image = image_with(b"payload-bytes");
  let mut buf = vec![0u8; CONTENT_LEN];
  image.write_to(&mut buf).unwrap();
  assert!(
    VolumeImage::read_from(&buf).unwrap().is_some(),
    "the well-formed framed object reads back"
  );

  // A frame length of u32::MAX in the committed slot's header: `unframe` checks the length against
  // the slot before decoding, so it refuses without allocating gigabytes. With the other slot empty,
  // recovery reads None — never a hang or an out-of-memory abort.
  let mut huge = buf.clone();
  huge[..4].copy_from_slice(&u32::MAX.to_le_bytes());
  assert!(
    !matches!(VolumeImage::read_from(&huge), Ok(Some(_))),
    "a u32::MAX frame length refuses without allocating, never a garbage success"
  );

  // Every byte flipped: every slot's CRC fails, so nothing is committed — never a garbage decode.
  let mut flipped = buf.clone();
  for b in flipped.iter_mut() {
    *b ^= 0xA5;
  }
  assert!(
    !matches!(VolumeImage::read_from(&flipped), Ok(Some(_))),
    "bit-flipped content commits nothing"
  );

  // A single byte flipped inside the committed slot's payload: the CRC catches it and, with no other
  // valid slot, recovery reads None (the caller then refuses) rather than a torn image.
  let mut torn = buf.clone();
  torn[16] ^= 0xFF;
  assert!(
    !matches!(VolumeImage::read_from(&torn), Ok(Some(_))),
    "a payload byte corrupted under a valid length is caught by the CRC"
  );
}

/// A hostile-input gate on the *rebuild* (`from_image`): an image that decodes as well-formed `Wire`
/// but is semantically malformed — a directory entry naming an inode that is not in the image — is
/// refused with a typed [`VfsError::RecoveryIncomplete`], never a panic and never a half-built volume
/// (referential integrity is checked as the tree is rebuilt, so a crafted image cannot corrupt).
#[test]
fn a_semantically_malformed_image_is_refused_by_the_rebuild() {
  let mut image = image_with(b"x");
  let root_no = image.root_no;
  // Craft a dangling reference: the root directory gains an entry naming an inode that does not exist.
  for inode in &mut image.inodes {
    if inode.no == root_no
      && let BodyImage::Directory { entries, .. } = &mut inode.body
    {
      entries.push(EntryImage {
        name: "dangling".to_string(),
        child: Some(999_999),
      });
    }
  }
  let mut fresh = common::store();
  let claims = common::claims(&mut fresh, &[&image]);
  assert!(
    matches!(
      Volume::from_image(
        &mut fresh,
        &image,
        &claims,
        Box::new(StepClock::new(0, 1)),
        1 << 16,
        None
      ),
      Err(VfsError::RecoveryIncomplete)
    ),
    "a dangling directory reference is refused by the rebuild, not panicked or half-built"
  );
}

/// AC (§4.8; the barrier crash of 2026-09-15,
/// docs/bugs/2026-09-15-recovery-image-materializes-a-sparse-files-holes.md): a sparse file images
/// as what it holds, never as its logical length. Do: hold four bytes at offset zero, then
/// extend the file to 999,999,999,999,999 bytes (pjdfstest's `truncate/12.t`). Expect: the image
/// carries the size and fits in a few hundred bytes (non-vacuity: a
/// materialized hole would be a petabyte, and was — the daemon aborted on the allocation); a volume
/// rebuilt from it serves the four bytes, reports the size, and reads zeros in the hole.
#[test]
fn a_sparse_file_images_as_what_it_holds_not_its_length() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let f = vol
    .create_file_no(&mut store, root, "sparse", 0o644)
    .unwrap();
  vol.write(&mut store, f, 0, b"held").unwrap();
  vol.truncate(&mut store, f, 999_999_999_999_999).unwrap();

  let image = vol.to_image(&store, None).unwrap();
  let sparse = image.inodes.iter().find(|i| i.no == f.0).unwrap();
  assert_eq!(sparse.attrs.size, 999_999_999_999_999);
  let content = image.to_content();
  assert!(
    content.len() < 4096,
    "the image is bounded by the bytes held, not the length: {} bytes",
    content.len()
  );

  let mut fresh = common::surviving(&store);
  let image = VolumeImage::from_content(&content).unwrap();
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
    rebuilt.stat(&fresh, f).unwrap().size,
    999_999_999_999_999,
    "the rebuilt file keeps its size"
  );
  let mut head = [0u8; 4];
  assert_eq!(rebuilt.read(&fresh, f, 0, &mut head).unwrap(), 4);
  assert_eq!(&head, b"held");
  let mut hole = [1u8; 16];
  assert_eq!(
    rebuilt
      .read(&fresh, f, 999_999_999_999_000, &mut hole)
      .unwrap(),
    16
  );
  assert!(
    hole.iter().all(|b| *b == 0),
    "the hole reads as zeros after the rebuild"
  );
}

/// A file with a hole in the middle images as what it holds on either side and rebuilds with the hole
/// intact, so an image is bounded by what is held, not by the span it lies in.
#[test]
fn a_file_with_a_middle_hole_images_as_what_it_holds() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let f = vol
    .create_file_no(&mut store, root, "holey", 0o644)
    .unwrap();
  let far = 8u64 << 20;
  vol.write(&mut store, f, 0, b"head").unwrap();
  vol.write(&mut store, f, far, b"tail").unwrap();

  let image = vol.to_image(&store, None).unwrap();
  let content = image.to_content();
  assert!(
    content.len() < 4096,
    "eight megabytes of hole cost nothing: {} bytes",
    content.len()
  );

  let mut fresh = common::surviving(&store);
  let image = VolumeImage::from_content(&content).unwrap();
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
  assert_eq!(rebuilt.stat(&fresh, f).unwrap().size, far + 4);
  let mut tail = [0u8; 4];
  assert_eq!(rebuilt.read(&fresh, f, far, &mut tail).unwrap(), 4);
  assert_eq!(&tail, b"tail");
  let mut middle = [1u8; 8];
  assert_eq!(rebuilt.read(&fresh, f, far / 2, &mut middle).unwrap(), 8);
  assert!(
    middle.iter().all(|b| *b == 0),
    "the middle hole reads as zeros"
  );
}

/// A-61 (AC-3.4: a FUSE mount the anchor held keeps its kernel's references across a restart). Do: in one
/// volume, a recorded attachment (42) references a linked file and an unlinked-but-open one; a second recorded
/// attachment (99) and an owner this process alone knows (7) each hold another orphan; rebuild the volume from
/// its image and settle it with only 42 surviving. Expect: the image carries the recorded attachments' references
/// and not the process's; the settle releases 99's and the process's share and reclaims their two orphans, keeps
/// 42's orphan readable, and 42's own forget then reclaims it — no orphan leaks and none is lost early.
#[test]
fn a_surviving_attachments_references_are_restored_and_every_other_holders_released() {
  use slates_vfs::ids::RefOwner;
  let mut src = store();
  let mut vol = volume(&mut src, 1 << 30);
  let root = vol.root_inode(&src).unwrap();
  let make = |vol: &mut Volume, src: &mut slates_vfs::volume::Store, name: &str| {
    let no = vol.create_file_no(src, root, name, 0o644).unwrap();
    vol.write(src, no, 0, name.as_bytes()).unwrap();
    no
  };
  let linked = make(&mut vol, &mut src, "linked");
  let kept = make(&mut vol, &mut src, "kept");
  let dropped = make(&mut vol, &mut src, "dropped");
  let local = make(&mut vol, &mut src, "local");
  vol
    .reference_for(&src, linked, RefOwner::Attachment(42))
    .unwrap();
  vol
    .reference_for(&src, kept, RefOwner::Attachment(42))
    .unwrap();
  vol
    .reference_for(&src, dropped, RefOwner::Attachment(99))
    .unwrap();
  vol
    .reference_for(&src, local, RefOwner::Process(7))
    .unwrap();
  for name in ["kept", "dropped", "local"] {
    vol.unlink_no(&mut src, root, name).unwrap();
  }
  let image = vol.to_image(&src, None).unwrap();
  let carried: Vec<u64> = image
    .references
    .iter()
    .map(|owner| owner.attachment)
    .collect();
  assert_eq!(
    carried,
    [42, 99],
    "the recorded attachments' references, never the process's"
  );
  let mut fresh = common::surviving(&src);
  let claims = common::claims(&mut fresh, &[&image]);
  let mut recovered = Volume::from_image(
    &mut fresh,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  let settled = recovered
    .settle_recovered_references(&mut fresh, |id| id == 42)
    .unwrap();
  assert_eq!(
    settled,
    (1, 2),
    "99 swept; the orphans 99 and the process held reclaimed"
  );
  let mut buf = [0u8; 16];
  let readable = |vol: &Volume, store: &slates_vfs::volume::Store, no| {
    vol.read(store, no, 0, &mut [0u8; 16]).is_ok()
  };
  assert!(
    readable(&recovered, &fresh, kept),
    "42's orphan is still held"
  );
  assert!(
    !readable(&recovered, &fresh, dropped),
    "99 did not survive: its orphan is reclaimed"
  );
  assert!(
    !readable(&recovered, &fresh, local),
    "the process's orphan died with it"
  );
  let n = recovered.read(&fresh, linked, 0, &mut buf).unwrap();
  assert_eq!(buf.get(..n), Some(&b"linked"[..]));
  recovered
    .forget_for(&mut fresh, kept, RefOwner::Attachment(42), u64::MAX)
    .unwrap();
  assert!(
    !readable(&recovered, &fresh, kept),
    "42's forget reclaims its orphan"
  );
}

/// A-61, hostile input. Do: rebuild from an image whose recorded references name an inode the image lacks, and
/// from one recording a zero count. Expect: both refused `RecoveryIncomplete`, never a volume with a reference
/// to nothing or an entry no forget can drop.
#[test]
fn an_image_with_a_corrupt_reference_record_is_refused() {
  use slates_vfs::recover::{AttachmentReferences, InodeReferences};
  let mut src = store();
  let mut vol = volume(&mut src, 1 << 30);
  let root = vol.root_inode(&src).unwrap();
  let f = vol.create_file_no(&mut src, root, "f", 0o644).unwrap();
  let image = vol.to_image(&src, None).unwrap();
  let corrupt = |inode: u64, count: u32| {
    let mut image = image.clone();
    image.references = vec![AttachmentReferences {
      attachment: 1,
      inodes: vec![InodeReferences { inode, count }],
    }];
    image
  };
  for (inode, count, what) in [
    (f.0 + 1_000, 1, "an absent inode"),
    (f.0, 0, "a zero count"),
  ] {
    let mut fresh = common::surviving(&src);
    let claims = common::claims(&mut fresh, &[&corrupt(inode, count)]);
    let rebuilt = Volume::from_image(
      &mut fresh,
      &corrupt(inode, count),
      &claims,
      Box::new(StepClock::new(0, 1)),
      1 << 16,
      None,
    );
    assert!(
      matches!(rebuilt, Err(VfsError::RecoveryIncomplete)),
      "{what} must be refused"
    );
  }
}

/// A-61 (the exact replay's record). Do: a shard image carrying two barrier replies, given out of attachment order;
/// encode it twice and decode it. Expect: identical bytes both times (a determinism gate) and the replies back in
/// attachment order, each with its unique and bytes.
#[test]
fn a_shard_image_carries_its_undelivered_replies_in_a_canonical_order() {
  use slates_vfs::recover::{HeldReply, ShardImage};
  let reply = |attachment: u64, unique: u64| HeldReply {
    attachment,
    unique,
    reply: vec![16, 0, 0, 0, 0, 0, 0, 0],
  };
  let image = ShardImage::new(Vec::new()).with_replies(vec![reply(9, 70), reply(2, 31)]);
  let bytes = image.to_content();
  assert_eq!(
    bytes,
    ShardImage::new(Vec::new())
      .with_replies(vec![reply(2, 31), reply(9, 70)])
      .to_content()
  );
  let decoded = ShardImage::from_content(&bytes).unwrap();
  assert_eq!(decoded.replies, [reply(2, 31), reply(9, 70)]);
}

/// Image memory that refuses every write past `budget` bytes: a publish torn by a crash partway.
struct TornAfter {
  bytes: Vec<u8>,
  budget: usize,
}

impl slates_vfs::recover::ImageRead for TornAfter {
  fn image_len(&self) -> usize {
    self.bytes.len()
  }

  fn image_read(&self, offset: usize, into: &mut [u8]) -> Result<(), VfsError> {
    self.bytes.image_read(offset, into)
  }
}

impl slates_vfs::recover::ImageWrite for TornAfter {
  fn image_write(&mut self, offset: usize, from: &[u8]) -> Result<(), VfsError> {
    let take = from.len().min(self.budget);
    self.bytes.image_write(offset, &from[..take])?;
    self.budget -= take;
    if take < from.len() {
      return Err(VfsError::NoSpace);
    }
    Ok(())
  }
}

/// §4.8 (a publisher that keeps its committed slot). Do: publish three shard images in turn, each after the slot the
/// last one returned; then tear a fourth partway (its memory refuses writes past a few bytes) and publish a fifth
/// after the same kept slot. Expect: each publish reads back with generations 1, 2, 3; the torn one leaves the third
/// readable and the kept slot unchanged; the fifth reads back as generation 4, written over the torn slot, never the
/// committed one.
#[test]
fn a_publisher_that_keeps_its_committed_slot_publishes_in_one_pass_and_survives_a_torn_write() {
  let image = |content: &[u8]| {
    ShardImage::new(vec![KeyedImage {
      key: [1; 16],
      image: image_with(content),
    }])
  };
  let mut memory = TornAfter {
    bytes: vec![0u8; CONTENT_LEN],
    budget: usize::MAX,
  };
  let mut known = None;
  for (generation, content) in [(1u64, b"one".as_slice()), (2, b"two"), (3, b"three")] {
    let shard = image(content);
    let (_, committed) = shard.write_after(&mut memory, known).unwrap();
    assert_eq!(committed.generation, generation);
    assert_eq!(
      slates_vfs::recover::committed_slot(&memory),
      Some(committed)
    );
    assert_eq!(ShardImage::read_from(&memory).unwrap(), Some(shard));
    known = Some(committed);
  }
  memory.budget = 16;
  assert!(image(b"torn").write_after(&mut memory, known).is_err());
  assert_eq!(
    ShardImage::read_from(&memory).unwrap(),
    Some(image(b"three"))
  );
  assert_eq!(slates_vfs::recover::committed_slot(&memory), known);
  memory.budget = usize::MAX;
  let (_, after) = image(b"five").write_after(&mut memory, known).unwrap();
  assert_eq!(after.generation, 4);
  assert_eq!(
    ShardImage::read_from(&memory).unwrap(),
    Some(image(b"five"))
  );
}

/// The volumes the streamed encoders are checked on: the built volume (a tree, an inline file, a multi-chunk file, a
/// symlink, a hard link), one with an attribute, a snapshot taken and the head changed after it, and an open orphan;
/// and an origin with a clone of its snapshot that diverged.
fn varied_volumes(store: &mut Store) -> Vec<([u8; 16], Volume)> {
  let mut plain = volume(store, 1 << 30);
  let root = plain.root_inode(store).unwrap();
  let dir = plain.mkdir_no(store, root, "dir", 0o755).unwrap();
  let small = plain.create_file_no(store, root, "small", 0o644).unwrap();
  plain.write(store, small, 0, b"hello").unwrap();
  let big = plain.create_file_no(store, dir, "big", 0o600).unwrap();
  let big_bytes: Vec<u8> = (0..256u32 * 1024).map(pattern_byte).collect();
  plain.write(store, big, 0, &big_bytes).unwrap();
  plain.symlink_no(store, root, "link", "dir/big").unwrap();
  plain.link_no(store, dir, "small_alias", small).unwrap();
  plain
    .xattr_set(
      store,
      small,
      b"com.apple.provenance",
      b"value-bytes",
      slates_vfs::xattr::XattrSet::Either,
    )
    .unwrap();
  let _snap = plain.snapshot(store).unwrap();
  plain
    .write(store, small, 0, b"HELLO, after the snapshot")
    .unwrap();
  let orphan = plain.create_file_no(store, root, "orphan", 0o644).unwrap();
  plain.write(store, orphan, 0, b"orphan-bytes").unwrap();
  plain.reference(store, orphan).unwrap();
  plain.unlink_no(store, root, "orphan").unwrap();

  let mut origin = prefixed_volume(store, 7);
  let origin_root = origin.root_inode(store).unwrap();
  let shared = origin
    .create_file_no(store, origin_root, "shared", 0o644)
    .unwrap();
  origin.write(store, shared, 0, b"from the origin").unwrap();
  let snap = origin.snapshot(store).unwrap();
  let mut clone = Volume::clone_of(
    store,
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
  let clone_root = clone.root_inode(store).unwrap();
  let own = clone
    .create_file_no(store, clone_root, "clone-only", 0o644)
    .unwrap();
  clone.write(store, own, 0, b"diverged").unwrap();
  vec![
    (key_bytes(3), plain),
    (key_bytes(1), origin),
    (key_bytes(2), clone),
  ]
}

/// The streamed checkpoint (2026-10-06). Do: encode varied volumes — a tree with an inline file, a multi-chunk file, a
/// symlink, a hard link, an attribute, a snapshot with the head changed after it, an open orphan, an origin and its
/// diverged clone — with `Volume::encode_image`, and against `to_image().encode()`. Expect: the same bytes, every one,
/// and the bytes decode back to the image.
#[test]
fn a_streamed_volume_image_is_byte_for_byte_the_image_encoding() {
  let mut store = store();
  for (key, vol) in varied_volumes(&mut store) {
    let image = vol.to_image(&store, None).unwrap();
    let mut streamed = Vec::new();
    vol.encode_image(&store, None, &mut streamed).unwrap();
    assert_eq!(
      streamed,
      image.to_content(),
      "volume {key:?}: streamed bytes"
    );
    assert_eq!(
      VolumeImage::from_content(&streamed).unwrap(),
      image,
      "volume {key:?}: decodes back"
    );
  }
}

/// A shard's volumes and the held bytes and replies a streamed checkpoint carries, with one volume of
/// [`STREAMED_FILES`] files so the image is several stages long.
fn streamed_shard(store: &mut Store) -> StreamedShard {
  let mut volumes = varied_volumes(store);
  let mut big = volume(store, STREAMED_QUOTA);
  let root = big.root_inode(store).unwrap();
  for file in 0..STREAMED_FILES {
    let no = big
      .create_file_no(store, root, &format!("file-{file:05}"), 0o644)
      .unwrap();
    big.write(store, no, 0, b"streamed bytes").unwrap();
  }
  volumes.push((key_bytes(4), big));
  volumes.sort_by_key(|(key, _)| *key);
  let replies = vec![
    slates_vfs::recover::HeldReply {
      attachment: 9,
      unique: 1,
      reply: b"r9".to_vec(),
    },
    slates_vfs::recover::HeldReply {
      attachment: 4,
      unique: 2,
      reply: b"r4".to_vec(),
    },
  ];
  (volumes, b"held replica image bytes".to_vec(), replies)
}

/// A streamed shard's volumes by key, its held bytes and its replies.
type StreamedShard = (
  Vec<([u8; 16], Volume)>,
  Vec<u8>,
  Vec<slates_vfs::recover::HeldReply>,
);

/// Shape: the files of the streamed checkpoint's large volume: enough that its image spans several stages.
const STREAMED_FILES: usize = 2_000;
/// Shape: the large volume's quota: well above its files' charge (about 2.4 KB an inode, A-109, plus their bytes).
const STREAMED_QUOTA: u64 = 1 << 26;

/// Streams the shard into `journal`'s checkpoint over `slots`: `encode_start`, each volume's key then
/// `encode_image_into` in key order, `encode_finish`, `finish_checkpoint`.
fn stream_into(
  journal: &mut slates_vfs::checkpoint_log::Journal,
  slots: &mut Vec<u8>,
  store: &Store,
  volumes: &[([u8; 16], Volume)],
  held: &[u8],
  replies: &mut Vec<slates_vfs::recover::HeldReply>,
) -> Result<usize, VfsError> {
  let mut stage = Vec::new();
  let mut stream = journal.begin_checkpoint(slots, &mut stage)?;
  ShardImage::encode_start(stream.buf(), volumes.len());
  for (key, vol) in volumes {
    stream.buf().extend_from_slice(key);
    vol.encode_image_into(store, None, &mut stream)?;
  }
  ShardImage::encode_finish(stream.buf(), held, replies);
  journal.finish_checkpoint(stream)
}

/// The expected image of a streamed shard.
fn shard_of(
  store: &Store,
  volumes: &[([u8; 16], Volume)],
  held: &[u8],
  replies: &[slates_vfs::recover::HeldReply],
) -> ShardImage {
  ShardImage::new(
    volumes
      .iter()
      .map(|(key, vol)| KeyedImage {
        key: *key,
        image: vol.to_image(store, None).unwrap(),
      })
      .collect(),
  )
  .with_held(held.to_vec())
  .with_replies(replies.to_vec())
}

/// The streamed checkpoint (2026-10-06). Do: stream a shard — the varied volumes plus one of 2,000 files, so the image
/// is several stages long — straight into a journal's checkpoint slot, with held bytes and replies, and recover it.
/// Expect: the frame is exactly the header, the generation and `ShardImage::new(…).with_held(…).with_replies(…)`'s
/// encoding, and the journal recovers that shard image; the image is longer than two stages (non-vacuity: the stream
/// flushed mid-image).
#[test]
fn a_streamed_shard_checkpoint_equals_the_shard_image_and_recovers() {
  let mut store = store();
  let (volumes, held, mut replies) = streamed_shard(&mut store);
  let expected = shard_of(&store, &volumes, &held, &replies);
  let encoded = expected.to_content();
  assert!(
    encoded.len() > 2 * slates_vfs::recover::STREAM_STAGE_BYTES,
    "the image spans stages: {} bytes",
    encoded.len()
  );
  let mut slots = vec![0u8; CONTENT_LEN];
  let mut journal = slates_vfs::checkpoint_log::Journal::default();
  let frame = stream_into(
    &mut journal,
    &mut slots,
    &store,
    &volumes,
    &held,
    &mut replies,
  )
  .unwrap();
  assert_eq!(frame, FRAME_BYTES + encoded.len(), "the frame's bytes");
  assert_eq!(
    slots.get(FRAME_BYTES..frame),
    Some(&encoded[..]),
    "the streamed image bytes"
  );
  let (recovered, _) =
    slates_vfs::checkpoint_log::Journal::recover(&slots, &Vec::<u8>::new()).unwrap();
  assert_eq!(
    recovered,
    Some(expected),
    "the journal recovers the shard image"
  );
}

/// Format: a checkpoint frame's bytes before its image: the length and CRC header and the generation.
const FRAME_BYTES: usize = 16;

/// A streamed checkpoint that cannot commit (2026-10-06). Do: commit a small checkpoint, then stream the large shard
/// into slots that hold the small image but not the large one, and separately begin a stream and drop it partway.
/// Expect: the large stream is refused `NoSpace` with the stream's own refusal recorded (so a publisher tells it from
/// a volume that could not be captured), and after both the journal still recovers the small checkpoint: an abandoned
/// stream's slot never commits and the committed slot is never touched.
#[test]
fn a_refused_or_abandoned_streamed_checkpoint_leaves_the_committed_one() {
  let mut store = store();
  let (volumes, _, _) = streamed_shard(&mut store);
  let mut small_sorted = varied_volumes(&mut store);
  small_sorted.sort_by_key(|(key, _)| *key);
  let small_image = shard_of(&store, &small_sorted, &[], &[]);
  let slot_bytes = small_image.to_content().len() + slates_vfs::recover::STREAM_STAGE_BYTES;
  let mut slots = vec![0u8; 2 * slot_bytes];
  let mut journal = slates_vfs::checkpoint_log::Journal::default();
  stream_into(
    &mut journal,
    &mut slots,
    &store,
    &small_sorted,
    &[],
    &mut Vec::new(),
  )
  .unwrap();

  let mut stage = Vec::new();
  let mut stream = journal.begin_checkpoint(&mut slots, &mut stage).unwrap();
  ShardImage::encode_start(stream.buf(), volumes.len());
  let mut refusal = None;
  for (key, vol) in &volumes {
    stream.buf().extend_from_slice(key);
    if let Err(e) = vol.encode_image_into(&store, None, &mut stream) {
      refusal = Some(e);
      break;
    }
  }
  assert_eq!(
    refusal,
    Some(VfsError::NoSpace),
    "the slot refuses the image"
  );
  assert_eq!(
    stream.refused(),
    Some(&VfsError::NoSpace),
    "the refusal is the stream's"
  );
  let (recovered, _) =
    slates_vfs::checkpoint_log::Journal::recover(&slots, &Vec::<u8>::new()).unwrap();
  assert_eq!(recovered.as_ref(), Some(&small_image), "after a refusal");

  let mut stage = Vec::new();
  let mut stream = journal.begin_checkpoint(&mut slots, &mut stage).unwrap();
  ShardImage::encode_start(stream.buf(), volumes.len());
  if let Some((key, vol)) = volumes.first() {
    stream.buf().extend_from_slice(key);
    vol.encode_image_into(&store, None, &mut stream).unwrap();
  }
  let (recovered, _) =
    slates_vfs::checkpoint_log::Journal::recover(&slots, &Vec::<u8>::new()).unwrap();
  assert_eq!(
    recovered.as_ref(),
    Some(&small_image),
    "after an abandoned stream"
  );
}

/// Shape: the steps of the write-accounting history: enough that every kind of write meets every body shape.
const ACCOUNTING_STEPS: usize = 300;
/// Shape: the windows the write-accounting file spans.
const ACCOUNTING_WINDOWS: u64 = 6;

/// The epoch histogram a write keeps (2026-10-06: a write reconciles only the windows it can change). Do: on a file
/// of six chunk windows, run a seeded history of overwrites (inside a window, across windows, far from the open
/// extent, a byte, a whole window) and truncates (into a window, to a window boundary, to zero, then growing again),
/// and after every step image the volume and rebuild it in a fresh store, which recounts its histogram from its
/// bodies. Expect: the live volume's referenced and unique bytes equal the rebuilt one's after every step, and its
/// bytes read back the same.
#[test]
fn a_writes_accounting_equals_a_recount_after_every_step() {
  let mut store = store();
  let mut vol = volume(&mut store, STREAMED_QUOTA);
  let root = vol.root_inode(&store).unwrap();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  let mut seed = 0x2545_F491_4F6C_DD1Du64;
  let mut next = move || {
    seed ^= seed << 13;
    seed ^= seed >> 7;
    seed ^= seed << 17;
    seed
  };
  for step in 0..ACCOUNTING_STEPS {
    let span = chunk * ACCOUNTING_WINDOWS;
    let choice = next() % 10;
    if choice < 7 {
      let off = next() % span;
      let len = match next() % 4 {
        0 => 1,
        1 => chunk,
        2 => chunk * 2 + 17,
        _ => 1 + next() % 4096,
      };
      let fill = u8::try_from(step % 251).unwrap();
      let bytes = vec![fill; usize::try_from(len).unwrap()];
      vol.write(&mut store, file, off, &bytes).unwrap();
    } else {
      let len = match next() % 3 {
        0 => 0,
        1 => (next() % ACCOUNTING_WINDOWS) * chunk,
        _ => next() % span,
      };
      vol.truncate(&mut store, file, len).unwrap();
    }
    let image = vol.to_image(&store, None).unwrap();
    let mut fresh = common::surviving(&store);
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
    let (live, recounted) = (vol.accounting(), rebuilt.accounting());
    assert_eq!(
      (live.referenced_bytes, live.unique_bytes),
      (recounted.referenced_bytes, recounted.unique_bytes),
      "step {step} ({choice})"
    );
  }
}
