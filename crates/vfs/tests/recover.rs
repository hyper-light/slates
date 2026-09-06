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
use slates_vfs::VfsError;
use slates_vfs::ids::InodeNo;
use slates_vfs::recover::{BodyImage, InodeImage, KindImage, PolicyImage, VolumeImage};

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
