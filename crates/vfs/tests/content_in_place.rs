//! Content resident once (A-64): a recovery image names each file's chunks and open extent by their blocks in
//! the shard's arena range, whose bytes survive the daemon in anchor RAM, and recovery claims exactly those
//! blocks. These tests drive the claim, the rebuild and the sweep through the volume's own verbs, with the
//! restart modelled by [`common::surviving`] (the arena's bytes carried into a fresh store, as the anchor's RAM
//! carries them into a restarted daemon).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{store, volume};
use slates_vfs::VfsError;
use slates_vfs::clock::StepClock;
use slates_vfs::ids::InodeNo;
use slates_vfs::recover::{
  BlockImage, BodyImage, Claims, ExtentSourceImage, InodeImage, OpenImage, VolumeImage,
};
use slates_vfs::volume::{Store, Volume};

/// Shape: a journal for the rebuilt volumes, as the other recovery tests give.
const JOURNAL_BYTES: usize = 1 << 16;

fn rebuild(fresh: &mut Store, image: &VolumeImage, claims: &Claims) -> Result<Volume, VfsError> {
  Volume::from_image(
    fresh,
    image,
    claims,
    Box::new(StepClock::new(0, 1)),
    JOURNAL_BYTES,
    None,
  )
}

/// A file's whole bytes through the read path.
fn read_all(vol: &Volume, store: &Store, no: InodeNo, len: usize) -> Vec<u8> {
  let mut out = vec![0u8; len];
  let mut read = 0;
  while read < len {
    let n = vol
      .read(store, no, u64::try_from(read).unwrap(), &mut out[read..])
      .unwrap();
    if n == 0 {
      break;
    }
    read += n;
  }
  out.truncate(read);
  out
}

/// The inode image numbered `no`.
fn inode_mut(image: &mut VolumeImage, no: InodeNo) -> &mut InodeImage {
  image.inodes.iter_mut().find(|i| i.no == no.0).unwrap()
}

/// The first chunk block a chunked body names.
fn first_chunk_block(image: &VolumeImage, no: InodeNo) -> BlockImage {
  let inode = image.inodes.iter().find(|i| i.no == no.0).unwrap();
  let BodyImage::Chunked { extents, .. } = &inode.body else {
    panic!("not chunked: {:?}", inode.body);
  };
  extents
    .iter()
    .find_map(|e| match e.source {
      ExtentSourceImage::Chunk { chunk, .. } => Some(chunk.block),
      ExtentSourceImage::Zero => None,
    })
    .unwrap()
}

/// A volume with two files of two chunks each, sealed, and its image.
fn two_sealed_files(store: &mut Store) -> (Volume, InodeNo, InodeNo, VolumeImage) {
  let mut vol = volume(store, 1 << 30);
  let root = vol.root_inode(store).unwrap();
  let chunk = store.content.chunk_bytes();
  let a = vol.create_file_no(store, root, "a", 0o644).unwrap();
  let b = vol.create_file_no(store, root, "b", 0o644).unwrap();
  vol.write(store, a, 0, &vec![b'a'; 2 * chunk]).unwrap();
  vol.write(store, b, 0, &vec![b'b'; 2 * chunk]).unwrap();
  // A snapshot seals the open windows on the next write; writing a byte back seals both files' windows.
  vol.snapshot(store).unwrap();
  vol.write(store, a, 0, b"a").unwrap();
  vol.write(store, b, 0, b"b").unwrap();
  let image = vol.to_image(store, None).unwrap();
  (vol, a, b, image)
}

/// A-64 hostile image. Do: take a valid image and make it name its blocks in ways no store could: one file's
/// chunk at another's block with a different used length, a block past the arena, a block misaligned inside
/// another, and a chunk's block named again as an open extent. Expect: each preparation refused
/// `RecoveryIncomplete`, and the fresh arena holds nothing afterwards (every claim given back).
#[test]
fn a_recovery_image_naming_impossible_blocks_is_refused_with_every_claim_given_back() {
  let mut source = store();
  let (_vol, a, b, image) = two_sealed_files(&mut source);
  let block = first_chunk_block(&image, a);
  let mut cases: Vec<(&str, VolumeImage)> = Vec::new();

  let mut twice = image.clone();
  if let BodyImage::Chunked { extents, .. } = &mut inode_mut(&mut twice, b).body
    && let Some(ExtentSourceImage::Chunk { chunk, .. }) = extents.first_mut().map(|e| &mut e.source)
  {
    chunk.block = block;
    chunk.used = chunk.used.saturating_sub(1);
  }
  cases.push(("one block named as two different chunks", twice));

  let mut far = image.clone();
  if let BodyImage::Chunked { extents, .. } = &mut inode_mut(&mut far, a).body
    && let Some(ExtentSourceImage::Chunk { chunk, .. }) = extents.first_mut().map(|e| &mut e.source)
  {
    chunk.block.offset = 1 << 40;
  }
  cases.push(("a block past the arena", far));

  let mut inside = image.clone();
  if let BodyImage::Chunked { extents, .. } = &mut inode_mut(&mut inside, b).body
    && let Some(ExtentSourceImage::Chunk { chunk, .. }) = extents.first_mut().map(|e| &mut e.source)
  {
    chunk.block = BlockImage {
      offset: block.offset + block.len / 2,
      len: block.len / 2,
      ..block
    };
    chunk.used = chunk.used.min(u32::try_from(block.len / 2).unwrap());
  }
  cases.push(("a block inside another", inside));

  let mut open = image.clone();
  if let BodyImage::Chunked { open: slot, .. } = &mut inode_mut(&mut open, b).body {
    *slot = Some(OpenImage {
      offset: 0,
      len: 1,
      block,
      born: 0,
    });
  }
  cases.push(("a chunk's block named again as an open extent", open));

  for (what, corrupt) in cases {
    let mut fresh = common::surviving(&source);
    let refused = Claims::prepare(&mut fresh, [&corrupt]);
    assert!(
      matches!(refused, Err(VfsError::RecoveryIncomplete)),
      "{what}: {refused:?}"
    );
    assert_eq!(
      fresh.content.allocated_bytes(),
      0,
      "{what}: every claim given back"
    );
  }
}

/// A-64 (the open extent written in place after its image). Do: write a file into its open extent, capture
/// the image, then write more bytes past the imaged length into the same open block (a write no barrier made
/// stable), and restart from the surviving arena. Expect: the file has its imaged length and bytes, and a write
/// that extends the open extent across the gap reads zeros where the unstable write was, never its bytes —
/// with no zeroing at recovery, because the write path zero-fills a gap it extends over.
#[test]
fn an_open_extent_comes_back_at_its_imaged_length_with_the_unstable_bytes_zeroed() {
  let mut source = store();
  let mut vol = volume(&mut source, 1 << 30);
  let root = vol.root_inode(&source).unwrap();
  let f = vol.create_file_no(&mut source, root, "f", 0o644).unwrap();
  let stable = vec![b's'; 1000];
  vol.write(&mut source, f, 0, &stable).unwrap();
  let image = vol.to_image(&source, None).unwrap();
  let inode = image.inodes.iter().find(|i| i.no == f.0).unwrap();
  assert!(
    matches!(&inode.body, BodyImage::Chunked { open: Some(_), .. }),
    "the file is in an open extent: {:?}",
    inode.body
  );
  vol.write(&mut source, f, 1000, &[b'u'; 1000]).unwrap();

  let mut fresh = common::surviving(&source);
  let claims = common::claims(&mut fresh, &[&image]);
  let mut recovered = rebuild(&mut fresh, &image, &claims).unwrap();
  assert_eq!(recovered.stat(&fresh, f).unwrap().size, 1000);
  assert_eq!(read_all(&recovered, &fresh, f, 1000), stable);
  // A write past the gap extends the same open extent across it: the gap must read as zeros.
  recovered.write(&mut fresh, f, 1999, b"e").unwrap();
  let extended = read_all(&recovered, &fresh, f, 2000);
  assert!(
    extended
      .get(1000..1999)
      .unwrap()
      .iter()
      .all(|byte| *byte == 0),
    "the unstable write's bytes are zeroed, never served"
  );
}

/// A volume of `prefix` on `store` holding one two-chunk file of `fill`, and its image.
fn one_file_volume(store: &mut Store, prefix: u16, fill: u8) -> (Volume, InodeNo, VolumeImage) {
  let mut vol = Volume::create(store, common::clone_config(prefix)).unwrap();
  let root = vol.root_inode(store).unwrap();
  let chunk = store.content.chunk_bytes();
  let file = vol.create_file_no(store, root, "file", 0o644).unwrap();
  vol.write(store, file, 0, &vec![fill; 2 * chunk]).unwrap();
  let image = vol.to_image(store, None).unwrap();
  (vol, file, image)
}

/// A-64 (the sweep). Do: image three volumes into one shard, each holding a two-chunk file; recover the first;
/// refuse the second after the claims (a directory entry naming an inode its image lacks); never rebuild the
/// third (an image the catalog does not hold); sweep. Expect: the sweep frees exactly the third's two blocks
/// (the refused volume's teardown freed its own), every free deferred while the recovered image names the
/// blocks, and once a publication commits the arena holds exactly the first volume's blocks, which read intact.
#[test]
fn the_sweep_frees_every_block_no_recovered_volume_reaches_once_a_publication_commits() {
  let mut source = store();
  let chunk = source.content.chunk_bytes();
  let (_first, kept, first_image) = one_file_volume(&mut source, 7, b'k');
  let first_blocks = source.content.allocated_bytes();
  let (_second, _, mut refused_image) = one_file_volume(&mut source, 8, b'r');
  let (_third, _, unrecorded_image) = one_file_volume(&mut source, 9, b'u');
  let refused_root = refused_image.root_no;
  if let BodyImage::Directory { entries, .. } =
    &mut inode_mut(&mut refused_image, InodeNo(refused_root)).body
  {
    entries.push(slates_vfs::recover::EntryImage {
      name: "dangling".to_string(),
      child: Some(999_999),
    });
  }

  let mut fresh = common::surviving(&source);
  let claims = common::claims(
    &mut fresh,
    &[&first_image, &refused_image, &unrecorded_image],
  );
  assert_eq!(claims.blocks(), 6, "two blocks for each of the three files");
  let rebuilt = rebuild(&mut fresh, &first_image, &claims).unwrap();
  assert!(matches!(
    rebuild(&mut fresh, &refused_image, &claims),
    Err(VfsError::RecoveryIncomplete)
  ));
  let freed = claims.sweep(&mut fresh, [&rebuilt]).unwrap();
  assert_eq!(freed, 2, "the unrecorded volume's two blocks");
  assert_eq!(
    fresh.content.arena().deferred_bytes(),
    4 * chunk,
    "the refused and the unrecorded volumes' blocks wait: the recovered image names them"
  );
  fresh.content.arena_mut().capture();
  fresh.content.arena_mut().commit_capture();
  assert_eq!(
    fresh.content.allocated_bytes(),
    first_blocks,
    "only the rebuilt volume's blocks remain once a publication commits"
  );
  assert!(read_all(&rebuilt, &fresh, kept, 2 * chunk) == vec![b'k'; 2 * chunk]);
}

/// A-64 (sharing across volumes). Do: write a file, snapshot, clone the snapshot into a second volume on the
/// same store, image both, and recover both from the surviving arena. Expect: one claim per block (the clone
/// names its origin's chunks), the recovered store holds exactly what the live one did, and the clone reads its
/// origin's bytes.
#[test]
fn a_clone_and_its_origin_recover_sharing_their_chunks() {
  let mut source = store();
  let mut origin = volume(&mut source, 1 << 30);
  let root = origin.root_inode(&source).unwrap();
  let chunk = source.content.chunk_bytes();
  let f = origin
    .create_file_no(&mut source, root, "f", 0o644)
    .unwrap();
  origin
    .write(&mut source, f, 0, &vec![b'o'; 2 * chunk])
    .unwrap();
  let snapshot = origin.snapshot(&mut source).unwrap();
  let clone = Volume::clone_of(&source, &mut origin, snapshot, common::clone_config(8)).unwrap();
  let origin_image = origin.to_image(&source, None).unwrap();
  let clone_image = clone.to_image(&source, None).unwrap();

  let mut fresh = common::surviving(&source);
  let claims = common::claims(&mut fresh, &[&origin_image, &clone_image]);
  let recovered_origin = rebuild(&mut fresh, &origin_image, &claims).unwrap();
  let recovered_clone = rebuild(&mut fresh, &clone_image, &claims).unwrap();
  assert_eq!(
    fresh.content.allocated_bytes(),
    source.content.allocated_bytes(),
    "the clone's chunks are its origin's, claimed once"
  );
  assert!(read_all(&recovered_clone, &fresh, f, 2 * chunk) == vec![b'o'; 2 * chunk]);
  assert!(read_all(&recovered_origin, &fresh, f, 2 * chunk) == vec![b'o'; 2 * chunk]);
}

/// A-64 (`PublishNeeded`). Do: in a 16 MiB arena, write a 6 MiB file, commit a publication (its image names the
/// blocks), rewrite the file whole (every old block's free deferred), then write a second 6 MiB file. Expect: the
/// second write lands short — the windows the free arena held — and the rest is refused `PublishNeeded` (the room
/// exists, waiting on a publication), not a memory refusal; once a publication commits, the rest lands and both
/// files read back.
#[test]
fn an_arena_full_of_deferred_frees_refuses_publish_needed_until_a_publication_commits() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let size = 6 << 20;
  let first = vol
    .create_file_no(&mut store, root, "first", 0o644)
    .unwrap();
  vol.write(&mut store, first, 0, &vec![b'1'; size]).unwrap();
  store.content.arena_mut().capture();
  store.content.arena_mut().commit_capture();
  vol.write(&mut store, first, 0, &vec![b'2'; size]).unwrap();
  assert!(
    store.content.arena().deferred_bytes() >= size,
    "the rewrite's old blocks wait"
  );
  let second = vol
    .create_file_no(&mut store, root, "second", 0o644)
    .unwrap();
  let landed = vol.write(&mut store, second, 0, &vec![b'3'; size]).unwrap();
  assert!(
    landed > 0 && landed < size,
    "a short write of what the free arena held: {landed}"
  );
  let rest = vec![b'3'; size - landed];
  let at = u64::try_from(landed).unwrap();
  let refused = vol.write(&mut store, second, at, &rest);
  assert!(
    matches!(refused, Err(VfsError::PublishNeeded)),
    "{refused:?}"
  );
  store.content.arena_mut().capture();
  store.content.arena_mut().commit_capture();
  assert_eq!(
    vol.write(&mut store, second, at, &rest).unwrap(),
    rest.len()
  );
  assert!(read_all(&vol, &store, second, size) == vec![b'3'; size]);
  assert!(read_all(&vol, &store, first, size) == vec![b'2'; size]);
}
