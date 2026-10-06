//! Content resident once (A-64): a recovery image names each file's chunks and open extent by their blocks in
//! the shard's arena range, whose bytes survive the daemon in anchor RAM, and recovery claims exactly those
//! blocks. These tests drive the claim, the rebuild and the sweep through the volume's own verbs, with the
//! restart modelled by [`common::surviving`] (the arena's bytes carried into a fresh store, as the anchor's RAM
//! carries them into a restarted daemon).
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

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

fn rebuild_clone(
  fresh: &mut Store,
  image: &VolumeImage,
  claims: &Claims,
  origin: &Volume,
  snapshot: slates_vfs::ids::SnapshotId,
) -> Result<Volume, VfsError> {
  Volume::clone_from_image(
    fresh,
    image,
    claims,
    origin,
    snapshot,
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
    .find_map(|e| match &e.source {
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
/// names its origin's chunks), the recovered store holds exactly the chunk blocks the live one did, the unchanged
/// clone adds no inode record of its own (it shares its origin snapshot's), and the clone reads its origin's bytes.
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
  let origin_records = fresh.inodes.len();
  let recovered_clone = rebuild_clone(
    &mut fresh,
    &clone_image,
    &claims,
    &recovered_origin,
    snapshot,
  )
  .unwrap();
  assert_eq!(
    fresh.content.allocated_bytes(),
    source.content.allocated_bytes(),
    "the clone's chunks are its origin's, claimed once"
  );
  assert_eq!(
    fresh.inodes.len(),
    origin_records,
    "the unchanged clone adds no inode record: it shares its origin snapshot's"
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

/// Destroys `vol` to the end.
fn destroy_whole(vol: &mut Volume, store: &mut Store) {
  vol.destroy(store).unwrap();
  while !matches!(
    vol.destroy_step(store, u64::MAX).unwrap(),
    slates_vfs::volume::DestroyProgress::Done
  ) {}
}

/// Builds an origin with two files and a directory, snapshots it, clones the snapshot and changes one file in the
/// clone; then destroys the clone (unpinning), the snapshot and the origin. Returns the store's live inode records
/// and directory nodes left.
fn clone_lifecycle_leftovers(restart: bool) -> (usize, usize) {
  let mut source = store();
  let mut origin = volume(&mut source, 1 << 30);
  let root = origin.root_inode(&source).unwrap();
  let chunk = source.content.chunk_bytes();
  let dir = origin.mkdir_no(&mut source, root, "d", 0o755).unwrap();
  let kept = origin
    .create_file_no(&mut source, dir, "kept", 0o644)
    .unwrap();
  let changed = origin
    .create_file_no(&mut source, root, "changed", 0o644)
    .unwrap();
  origin
    .write(&mut source, kept, 0, &vec![b'k'; 2 * chunk])
    .unwrap();
  origin
    .write(&mut source, changed, 0, &vec![b'c'; chunk])
    .unwrap();
  let snapshot = origin.snapshot(&mut source).unwrap();
  let mut clone =
    Volume::clone_of(&source, &mut origin, snapshot, common::clone_config(8)).unwrap();
  clone
    .write(&mut source, changed, 0, b"clone's own")
    .unwrap();
  let (mut store, mut origin, mut clone) = if restart {
    let origin_image = origin.to_image(&source, None).unwrap();
    let clone_image = clone.to_image(&source, None).unwrap();
    let mut fresh = common::surviving(&source);
    let claims = common::claims(&mut fresh, &[&origin_image, &clone_image]);
    let origin = rebuild(&mut fresh, &origin_image, &claims).unwrap();
    let clone = rebuild_clone(&mut fresh, &clone_image, &claims, &origin, snapshot).unwrap();
    (fresh, origin, clone)
  } else {
    (source, origin, clone)
  };
  destroy_whole(&mut clone, &mut store);
  origin.unpin(snapshot).unwrap();
  origin.destroy_snapshot(&mut store, snapshot).unwrap();
  destroy_whole(&mut origin, &mut store);
  (store.inodes.len(), store.dirs.len())
}

/// A-64, §4.5 (a clone shares its origin snapshot's records). Do: run a clone's whole life — origin, snapshot,
/// clone with one change, destroy the clone, the snapshot and the origin — live, and again with both volumes
/// recovered from their images before the destroys. Expect: nothing left either way. A recovered clone that
/// rebuilt its own copies of the origin's records, born at the origin's epochs, would never free them: a clone's
/// destroy leaves everything born at or before its origin to the origin.
#[test]
fn a_recovered_clone_and_its_origin_leave_nothing_behind_when_destroyed() {
  assert_eq!(
    clone_lifecycle_leftovers(false),
    (0, 0),
    "the live lifecycle"
  );
  assert_eq!(
    clone_lifecycle_leftovers(true),
    (0, 0),
    "the recovered lifecycle"
  );
}

/// A deterministic test cipher (A-99): each byte XORed with a stream of its key, version, segment and place, and a
/// tag that sums the plaintext under the same stream, so a ciphertext read as plaintext differs and a tampered
/// segment is refused.
struct StreamCipher;

fn stream_byte(key: u32, version: u64, index: u32, at: usize) -> u8 {
  let mixed = u64::from(key)
    .wrapping_mul(0x9E37_79B9)
    .wrapping_add(version.wrapping_mul(0x85EB_CA6B))
    .wrapping_add(u64::from(index).wrapping_mul(0xC2B2_AE35))
    .wrapping_add(u64::try_from(at).unwrap());
  (mixed ^ (mixed >> 13)).to_le_bytes()[0] | 1
}

fn stream_tag(key: u32, version: u64, index: u32, plain: &[u8]) -> slates_vfs::content::Tag {
  let mut tag = slates_vfs::content::Tag::default();
  for (at, byte) in plain.iter().enumerate() {
    let width = tag.len();
    let slot = tag.get_mut(at % width).unwrap();
    *slot = slot.wrapping_add(*byte ^ stream_byte(key, version, index, at));
  }
  tag
}

impl slates_vfs::content::ChunkCipher for StreamCipher {
  fn seal(
    &self,
    key: u32,
    version: u64,
    index: u32,
    _: bool,
    segment: &mut [u8],
  ) -> Result<slates_vfs::content::Tag, VfsError> {
    let tag = stream_tag(key, version, index, segment);
    for (at, byte) in segment.iter_mut().enumerate() {
      *byte ^= stream_byte(key, version, index, at);
    }
    Ok(tag)
  }
  fn open(
    &self,
    key: u32,
    version: u64,
    index: u32,
    _: bool,
    segment: &mut [u8],
    tag: &slates_vfs::content::Tag,
  ) -> Result<(), VfsError> {
    for (at, byte) in segment.iter_mut().enumerate() {
      *byte ^= stream_byte(key, version, index, at);
    }
    if stream_tag(key, version, index, segment) != *tag {
      return Err(VfsError::Integrity);
    }
    Ok(())
  }
  fn identity(&self, key: u32) -> Option<slates_vfs::content::KeyIdentity> {
    let mut identity = slates_vfs::content::KeyIdentity::default();
    identity
      .get_mut(..4)
      .unwrap()
      .copy_from_slice(&key.to_le_bytes());
    Some(identity)
  }
  fn reference(&mut self, identity: &slates_vfs::content::KeyIdentity) -> Result<u32, VfsError> {
    Ok(u32::from_le_bytes(
      identity.get(..4).unwrap().try_into().unwrap(),
    ))
  }
  fn key_for_volume(&mut self, _: [u8; 16]) -> Result<u32, VfsError> {
    Ok(SEAL_KEY)
  }
}

/// Shape: the key the sealing tests' volumes seal under.
const SEAL_KEY: u32 = 7;

/// A-99 with A-64 (a seal after the image). Do: on a sealing store, write a file into its open extent, capture the
/// image (it names the open block, plaintext), then write past the first chunk window, which seals the first window,
/// and restart from the surviving arena with no publication between. Expect: the file reads back its imaged bytes,
/// never the ciphertext the seal left, and the second window's unstable bytes are gone. A seal that encrypted the
/// imaged block in place turned the image's plaintext open extent into ciphertext.
#[test]
fn a_seal_after_the_image_leaves_the_imaged_open_extent_readable_after_a_restart() {
  let mut source = store();
  source.content.set_cipher(Box::new(StreamCipher));
  let mut vol = volume(&mut source, 1 << 30);
  vol.set_seal_key(Some(SEAL_KEY));
  let root = vol.root_inode(&source).unwrap();
  let f = vol.create_file_no(&mut source, root, "f", 0o644).unwrap();
  let stable: Vec<u8> = (0..1000u32)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  vol.write(&mut source, f, 0, &stable).unwrap();
  // A publication, as the daemon's barrier makes one: capture, image, commit.
  source.content.arena_mut().capture();
  let image = vol.to_image(&source, None).unwrap();
  source.content.arena_mut().commit_capture();
  assert!(
    matches!(
      &image.inodes.iter().find(|i| i.no == f.0).unwrap().body,
      BodyImage::Chunked { open: Some(_), .. }
    ),
    "the image names the file's open extent"
  );
  let chunk = u64::try_from(source.content.chunk_bytes()).unwrap();
  let sealed_before = source.content.sealed();
  vol
    .write(&mut source, f, chunk, b"past the first window")
    .unwrap();
  assert!(
    source.content.sealed() > sealed_before,
    "the write sealed the first window (non-vacuous)"
  );
  assert_eq!(
    source.content.moved_out_of_image(),
    1,
    "the seal wrote the chunk to a new block, the image naming the old one"
  );
  let mut after = vec![0u8; 1000];
  vol.read(&source, f, 0, &mut after).unwrap();
  assert_eq!(after, stable, "the live volume reads the sealed chunk back");

  let mut fresh = common::surviving(&source);
  fresh.content.set_cipher(Box::new(StreamCipher));
  let claims = common::claims(&mut fresh, &[&image]);
  let recovered = rebuild(&mut fresh, &image, &claims).unwrap();
  assert_eq!(recovered.stat(&fresh, f).unwrap().size, 1000);
  assert_eq!(read_all(&recovered, &fresh, f, 1000), stable);
}

/// Shape: bytes a sealing test writes as one file's content: a marker no other byte sequence in the store repeats, so
/// finding it in the arena means the file's plaintext is there.
const MARKER: &[u8] = b"PLAINTEXT-MARKER-a99-idle-sweep-0123456789abcdef";

/// A sealing store and a sealing volume on it.
fn sealing_volume() -> (Store, Volume) {
  let mut source = store();
  source.content.set_cipher(Box::new(StreamCipher));
  let mut vol = volume(&mut source, 1 << 30);
  vol.set_seal_key(Some(SEAL_KEY));
  (source, vol)
}

/// Whether any region of the store's arena holds `needle` anywhere in its bytes, freed or live.
fn arena_holds(store: &Store, needle: &[u8]) -> bool {
  (0..store.content.arena().regions()).any(|index| {
    store
      .content
      .arena()
      .region(u16::try_from(index).unwrap())
      .is_some_and(|region| {
        region
          .bytes()
          .windows(needle.len())
          .any(|window| window == needle)
      })
  })
}

/// A file of `copies` markers, the marker's bytes repeated.
fn marked(copies: usize) -> Vec<u8> {
  MARKER.repeat(copies)
}

/// A-99 (the idle sweep). Do: write a file smaller than a chunk on a sealing volume, then sweep twice with no write
/// between. Expect: the first sweep seals nothing (the file was written in the tick it ends) and its plaintext is in
/// the arena; the second seals it, the arena no longer holds its plaintext anywhere, and it reads back whole.
#[test]
fn an_idle_file_smaller_than_a_chunk_is_sealed_by_the_second_sweep() {
  let (mut source, mut vol) = sealing_volume();
  let root = vol.root_inode(&source).unwrap();
  let f = vol.create_file_no(&mut source, root, "f", 0o644).unwrap();
  let content = marked(20);
  vol.write(&mut source, f, 0, &content).unwrap();
  let first = vol.seal_idle(&mut source, u64::MAX);
  assert_eq!(first.sealed, 0, "written in the tick the sweep ends");
  assert!(
    arena_holds(&source, MARKER),
    "the open extent holds plaintext"
  );
  let second = vol.seal_idle(&mut source, u64::MAX);
  assert_eq!(second.sealed, 1);
  assert_eq!(second.sealed_bytes, u64::try_from(content.len()).unwrap());
  assert!(
    !arena_holds(&source, MARKER),
    "no plaintext of the file is left in the arena"
  );
  assert_eq!(read_all(&vol, &source, f, content.len()), content);
}

/// A-99 (the idle sweep). Do: write a file, sweep, write it again, sweep, then sweep once more. Expect: the second
/// sweep leaves it open (written since the sweep before), the third seals it, and it reads back with both writes.
#[test]
fn a_file_written_again_between_sweeps_stays_open_until_it_idles_a_whole_tick() {
  let (mut source, mut vol) = sealing_volume();
  let root = vol.root_inode(&source).unwrap();
  let f = vol.create_file_no(&mut source, root, "f", 0o644).unwrap();
  let first_write = marked(4);
  vol.write(&mut source, f, 0, &first_write).unwrap();
  vol.seal_idle(&mut source, u64::MAX);
  let at = u64::try_from(first_write.len()).unwrap();
  vol.write(&mut source, f, at, b"appended").unwrap();
  assert_eq!(
    vol.seal_idle(&mut source, u64::MAX).sealed,
    0,
    "written since the sweep before"
  );
  assert_eq!(vol.seal_idle(&mut source, u64::MAX).sealed, 1);
  let mut whole = first_write.clone();
  whole.extend_from_slice(b"appended");
  assert_eq!(read_all(&vol, &source, f, whole.len()), whole);
}

/// A-99 (the idle sweep is bounded). Do: write eight files, sweep once to age them, then sweep with a budget of one
/// file's bytes. Expect: that sweep seals one and leaves seven for the next, and the next seals all seven.
#[test]
fn a_sweep_stops_at_its_byte_budget_and_the_next_one_finishes() {
  let (mut source, mut vol) = sealing_volume();
  let root = vol.root_inode(&source).unwrap();
  // Past the inline threshold (two cache lines), so each file holds an open extent.
  let content = marked(20);
  for index in 0..8 {
    let f = vol
      .create_file_no(&mut source, root, &format!("f{index}"), 0o644)
      .unwrap();
    vol.write(&mut source, f, 0, &content).unwrap();
  }
  vol.seal_idle(&mut source, u64::MAX);
  let budget = u64::try_from(content.len()).unwrap();
  let bounded = vol.seal_idle(&mut source, budget);
  assert_eq!((bounded.sealed, bounded.left), (1, 7));
  let rest = vol.seal_idle(&mut source, u64::MAX);
  assert_eq!((rest.sealed, rest.left), (7, 0));
  assert!(!arena_holds(&source, MARKER));
}

/// A-99 (the idle sweep across a restart). Do: write a file, publish an image (it names the open extent), restart
/// from the surviving arena, then sweep the recovered volume twice. Expect: the recovered file is sealed by the second
/// sweep, as a file written before the daemon's first sweep would be, and reads back whole.
#[test]
fn a_recovered_open_extent_is_sealed_by_the_idle_sweep() {
  let (mut source, mut vol) = sealing_volume();
  let root = vol.root_inode(&source).unwrap();
  let f = vol.create_file_no(&mut source, root, "f", 0o644).unwrap();
  let content = marked(10);
  vol.write(&mut source, f, 0, &content).unwrap();
  source.content.arena_mut().capture();
  let image = vol.to_image(&source, None).unwrap();
  source.content.arena_mut().commit_capture();

  let mut fresh = common::surviving(&source);
  fresh.content.set_cipher(Box::new(StreamCipher));
  let claims = common::claims(&mut fresh, &[&image]);
  let mut recovered = rebuild(&mut fresh, &image, &claims).unwrap();
  recovered.set_seal_key(Some(SEAL_KEY));
  recovered.seal_idle(&mut fresh, u64::MAX);
  assert_eq!(recovered.seal_idle(&mut fresh, u64::MAX).sealed, 1);
  assert_eq!(read_all(&recovered, &fresh, f, content.len()), content);
}

/// A-99 (zero on free). Do: write a file into its open extent (plaintext in the arena), then remove it with no image
/// naming its block. Expect: the arena no longer holds its plaintext anywhere, live or free.
#[test]
fn a_removed_files_plaintext_is_scrubbed_from_the_arena() {
  let (mut source, mut vol) = sealing_volume();
  let root = vol.root_inode(&source).unwrap();
  let f = vol.create_file_no(&mut source, root, "f", 0o644).unwrap();
  vol.write(&mut source, f, 0, &marked(20)).unwrap();
  assert!(
    arena_holds(&source, MARKER),
    "the open extent holds plaintext"
  );
  vol.unlink_no(&mut source, root, "f").unwrap();
  assert!(
    !arena_holds(&source, MARKER),
    "the freed block was scrubbed"
  );
}

/// A-99 (zero on free, deferred). Do: write a file, publish an image naming its open block, remove the file, then
/// publish again. Expect: the plaintext stays while the committed image names the block (a restart may read it) and is
/// gone once the next publication commits and releases the block.
#[test]
fn a_removed_files_imaged_plaintext_is_scrubbed_when_the_next_publication_commits() {
  let (mut source, mut vol) = sealing_volume();
  let root = vol.root_inode(&source).unwrap();
  let f = vol.create_file_no(&mut source, root, "f", 0o644).unwrap();
  vol.write(&mut source, f, 0, &marked(20)).unwrap();
  source.content.arena_mut().capture();
  vol.to_image(&source, None).unwrap();
  source.content.arena_mut().commit_capture();
  vol.unlink_no(&mut source, root, "f").unwrap();
  assert!(
    arena_holds(&source, MARKER),
    "the committed image still names the block: its bytes are kept"
  );
  source.content.arena_mut().capture();
  vol.to_image(&source, None).unwrap();
  source.content.arena_mut().commit_capture();
  assert!(
    !arena_holds(&source, MARKER),
    "released at the commit and scrubbed"
  );
}

/// A-68 with the recording rule (2026-10-05). Do: snapshot a volume (its next publication is always full), publish it,
/// then write a file. Expect: the volume is not clean, so a barrier publishes the write. A volume that records no
/// changes because its next publication is full anyway must never answer clean, or a barrier would skip a change.
#[test]
fn a_volume_that_records_no_changes_is_never_clean_after_a_write() {
  let mut source = store();
  let mut vol = volume(&mut source, 1 << 30);
  let root = vol.root_inode(&source).unwrap();
  let f = vol.create_file_no(&mut source, root, "f", 0o644).unwrap();
  vol.snapshot(&mut source).unwrap();
  vol.mark_published(&source);
  vol
    .write(&mut source, f, 0, b"changed after the publication")
    .unwrap();
  assert!(
    !vol.is_clean(&source),
    "a write after the publication is not clean"
  );
}
