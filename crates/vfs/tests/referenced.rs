//! A-645 P3 ruling 2 (§4.5, A-64, A-727): a read-only file whose content is references into the shard's content
//! store, read a run at a time through the store's seam and never assembled. The seam here is a map of objects, each
//! lent in windows as the server's `ContentStore` lends a sealed chunk's; the counters are the non-vacuity check: a
//! view of N texts read whole returns every byte (`bytes_read` = its length) and copies none (`bytes_copied` = 0), and
//! the same view over a store that can only copy counts every content byte, so the zero is not a dead counter.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::borrow::Cow;
use std::collections::BTreeMap;

use slates_vfs::NameEquivalence;
use slates_vfs::VfsError;
use slates_vfs::clock::StepClock;
use slates_vfs::ids::InodeNo;
use slates_vfs::quota::Quota;
use slates_vfs::recover::VolumeImage;
use slates_vfs::referenced::{
  Identity, Piece, ReferencedContent, ReferencedCounts, ReferencedHolds,
};
use slates_vfs::volume::{Store, Volume, VolumeConfig};

/// Shape: the bytes the stand-in store lends at once, as a sealed chunk's window; small, so every text spans several.
const WINDOW: usize = 97;

/// Shape: the texts the view references.
const TEXTS: usize = 64;

/// Shape: the bytes a reader asks for at once: prime, so reads start and end inside pieces and windows.
const READ_BYTES: usize = 4093;

/// A content store over a map: each object lent in [`WINDOW`]-byte runs, borrowed, or copied when `copies` is set;
/// its holds counted per identity.
#[derive(Default)]
struct MapStore {
  objects: BTreeMap<Identity, Vec<u8>>,
  holds: BTreeMap<Identity, u64>,
  copies: bool,
  refuse_hold: Option<Identity>,
}

impl MapStore {
  /// Adds `bytes` under an identity made from `seed`: the identity.
  fn put(&mut self, seed: usize, bytes: Vec<u8>) -> Identity {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&u64::try_from(seed).unwrap().to_le_bytes());
    self.objects.insert(id, bytes);
    id
  }

  /// The holds still taken, over every identity.
  fn held(&self) -> u64 {
    self.holds.values().sum()
  }
}

impl ReferencedContent for MapStore {
  fn run_at(&self, id: &Identity, len: u64, offset: u64) -> Result<Cow<'_, [u8]>, VfsError> {
    let object = self.objects.get(id).ok_or(VfsError::ReferenceUnresolved)?;
    let len = usize::try_from(len).map_err(|_| VfsError::ReferenceUnresolved)?;
    let offset = usize::try_from(offset).map_err(|_| VfsError::ReferenceUnresolved)?;
    if len != object.len() || offset >= len {
      return Err(VfsError::ReferenceUnresolved);
    }
    let end = ((offset / WINDOW + 1) * WINDOW).min(len);
    let run = object
      .get(offset..end)
      .ok_or(VfsError::ReferenceUnresolved)?;
    Ok(if self.copies {
      Cow::Owned(run.to_vec())
    } else {
      Cow::Borrowed(run)
    })
  }
}

impl ReferencedHolds for MapStore {
  fn hold(&mut self, id: &Identity) -> Result<(), VfsError> {
    if !self.objects.contains_key(id) || self.refuse_hold == Some(*id) {
      return Err(VfsError::ReferenceUnresolved);
    }
    *self.holds.entry(*id).or_default() += 1;
    Ok(())
  }

  fn release(&mut self, id: &Identity) -> Result<(), VfsError> {
    let count = self
      .holds
      .get_mut(id)
      .ok_or(VfsError::ReferenceUnresolved)?;
    *count = count.checked_sub(1).ok_or(VfsError::ReferenceUnresolved)?;
    Ok(())
  }
}

/// Text `n`: bytes no other text shares, between one byte and a little over four windows long.
fn text(n: usize) -> Vec<u8> {
  let len = 1 + (n * 37) % (4 * WINDOW + 5);
  (0..len)
    .map(|at| u8::try_from((n * 131 + at * 7) % 251).unwrap())
    .collect()
}

/// A view of [`TEXTS`] texts, each after a made header as an FSFS revision file's representation is: the pieces and the
/// bytes they show.
fn view(store: &mut MapStore) -> (Vec<Piece>, Vec<u8>) {
  let mut pieces = Vec::new();
  let mut expected = Vec::new();
  for n in 0..TEXTS {
    let header = format!("DELTA {n}\n").into_bytes();
    expected.extend_from_slice(&header);
    pieces.push(Piece::Made(header.into_boxed_slice()));
    let bytes = text(n);
    expected.extend_from_slice(&bytes);
    let len = u64::try_from(bytes.len()).unwrap();
    let id = store.put(n, bytes);
    pieces.push(Piece::Content { id, len });
  }
  pieces.push(Piece::Made(b"ENDREP\n".to_vec().into_boxed_slice()));
  expected.extend_from_slice(b"ENDREP\n");
  (pieces, expected)
}

/// Reads file `no` whole through `content`, [`READ_BYTES`] at a time.
fn read_whole(volume: &Volume, store: &Store, no: InodeNo, content: &MapStore) -> Vec<u8> {
  let mut out = Vec::new();
  let mut buf = vec![0u8; READ_BYTES];
  loop {
    let offset = u64::try_from(out.len()).unwrap();
    let read = volume
      .read_referenced(store, no, offset, &mut buf, content)
      .unwrap();
    if read == 0 {
      return out;
    }
    out.extend_from_slice(buf.get(..read).unwrap());
  }
}

/// A-645 P3 ruling 2. Do: make a referenced file of [`TEXTS`] texts with made headers between them over a store that
/// lends its windows; stat it; read it whole a prime number of bytes at a time. Expect: its size is the pieces' sum
/// before any read; the bytes are the headers and texts in order; one hold per
/// text; `bytes_read` the view's length and `bytes_copied` zero. Then over a store that only copies, expect the same
/// bytes and `bytes_copied` every content byte: the counter counts.
#[test]
fn a_view_of_n_texts_reads_whole_and_copies_no_byte() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  let mut content = MapStore::default();
  let (pieces, expected) = view(&mut content);
  let made: u64 = pieces
    .iter()
    .filter(|piece| matches!(piece, Piece::Made(_)))
    .map(Piece::len)
    .sum();
  let no = volume
    .create_referenced(&mut store, root, "rev", 0o444, pieces, &mut content)
    .unwrap();
  assert_eq!(
    volume.stat(&store, no).unwrap().size,
    u64::try_from(expected.len()).unwrap()
  );
  assert_eq!(content.held(), u64::try_from(TEXTS).unwrap());
  assert!(content.holds.values().all(|holds| *holds == 1));
  assert_eq!(read_whole(&volume, &store, no, &content), expected);
  let length = u64::try_from(expected.len()).unwrap();
  assert_eq!(
    volume.referenced_counts(),
    ReferencedCounts {
      bytes_read: length,
      bytes_copied: 0,
    }
  );
  content.copies = true;
  assert_eq!(read_whole(&volume, &store, no, &content), expected);
  assert_eq!(
    volume.referenced_counts(),
    ReferencedCounts {
      bytes_read: 2 * length,
      bytes_copied: length - made,
    }
  );
}

/// A-645 P3 ruling 2. Do: make a referenced file, then write it, truncate it, allocate in it and edit it. Expect each
/// refused `EROFS` and its bytes unchanged.
#[test]
fn every_change_to_a_referenced_file_is_refused_read_only() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  let mut content = MapStore::default();
  let (pieces, expected) = view(&mut content);
  let no = volume
    .create_referenced(&mut store, root, "rev", 0o444, pieces, &mut content)
    .unwrap();
  let refusals = [
    volume.write(&mut store, no, 0, b"x").map(|_| ()),
    volume.write(&mut store, no, 1 << 20, b"x").map(|_| ()),
    volume.truncate(&mut store, no, 0),
    volume.truncate(&mut store, no, 1 << 20),
    volume.allocate(&mut store, no, 0, 1 << 20),
    volume.edit(&mut store, no, 0, 1, b"y"),
  ];
  for refusal in refusals {
    assert_eq!(refusal, Err(VfsError::ReadOnly));
    assert_eq!(VfsError::ReadOnly.errno_name(), "EROFS");
  }
  assert_eq!(read_whole(&volume, &store, no, &content), expected);
}

/// A-645 P3 ruling 2. Do: read a referenced file through the plain read, which lends no store; then read only its first
/// made header that way. Expect the content refused `ReferenceUnresolved` (`EIO`), never zeros, and the header served.
#[test]
fn a_read_lent_no_store_refuses_content_and_serves_made_bytes() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  let mut content = MapStore::default();
  let (pieces, expected) = view(&mut content);
  let no = volume
    .create_referenced(&mut store, root, "rev", 0o444, pieces, &mut content)
    .unwrap();
  let mut buf = vec![0u8; READ_BYTES];
  assert_eq!(
    volume.read(&store, no, 0, &mut buf),
    Err(VfsError::ReferenceUnresolved)
  );
  let header = b"DELTA 0\n".len();
  let mut head = vec![0u8; header];
  assert_eq!(volume.read(&store, no, 0, &mut head), Ok(header));
  assert_eq!(head, expected[..header]);
}

/// A-645 P3 ruling 2. Do: make a referenced file whose tenth text's hold the store refuses; then one naming an identity
/// the store does not hold. Expect each create refused with the store's refusal, no name made, and no hold left.
#[test]
fn a_refused_hold_fails_the_create_with_nothing_held() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  let mut content = MapStore::default();
  let (pieces, _) = view(&mut content);
  let tenth = match &pieces[19] {
    Piece::Content { id, .. } => *id,
    Piece::Made(_) => panic!("piece 19 is the tenth text"),
  };
  content.refuse_hold = Some(tenth);
  assert_eq!(
    volume.create_referenced(&mut store, root, "rev", 0o444, pieces, &mut content),
    Err(VfsError::ReferenceUnresolved)
  );
  let unknown = vec![Piece::Content {
    id: [0xee; 32],
    len: 1,
  }];
  assert_eq!(
    volume.create_referenced(&mut store, root, "rev", 0o444, unknown, &mut content),
    Err(VfsError::ReferenceUnresolved)
  );
  assert_eq!(content.held(), 0);
  assert_eq!(
    volume.lookup_no(&store, root, "rev").map(|_| ()),
    Err(VfsError::NotFound)
  );
}

/// A-645 P3 ruling 2. Do: make two referenced files; destroy the volume; clone a snapshot of it; release its references;
/// destroy it again. Expect the first destroy and the clone refused `ReferencesHeld` (`EBUSY`) with every hold kept, the
/// release to let go of every hold, and the second destroy to succeed.
#[test]
fn the_volume_keeps_its_holds_until_released_and_refuses_a_destroy_before() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  let mut content = MapStore::default();
  let (pieces, _) = view(&mut content);
  volume
    .create_referenced(&mut store, root, "a", 0o444, pieces.clone(), &mut content)
    .unwrap();
  volume
    .create_referenced(&mut store, root, "b", 0o444, pieces, &mut content)
    .unwrap();
  assert_eq!(content.held(), 2 * u64::try_from(TEXTS).unwrap());
  let snapshot = volume.snapshot(&mut store).unwrap();
  let config = VolumeConfig {
    prefix: 8,
    names: NameEquivalence::Fold,
    quota: Quota::Bounded { limit: 1 << 24 },
    journal_bytes: 1 << 20,
    clock: Box::new(StepClock::new(0, 1_000)),
  };
  assert_eq!(
    Volume::clone_of(&store, &mut volume, snapshot, config).map(|_| ()),
    Err(VfsError::ReferencesHeld)
  );
  assert_eq!(volume.destroy(&mut store), Err(VfsError::ReferencesHeld));
  assert_eq!(VfsError::ReferencesHeld.errno_name(), "EBUSY");
  assert_eq!(content.held(), 2 * u64::try_from(TEXTS).unwrap());
  volume.release_references(&mut content).unwrap();
  assert_eq!(content.held(), 0);
  volume.destroy(&mut store).unwrap();
}

/// A-645 P3 ruling 2. Do: make a referenced file; take the volume's image; recover it beside the surviving store; read
/// the file through the store; take the owed holds through a successor's store, then release. Expect the image to carry
/// the pieces, the recovered file to read the same bytes, the ledger owed until held, and the release to let go of
/// exactly what the successor took.
#[test]
fn a_recovered_volume_reads_its_referenced_file_and_owes_its_holds() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  let mut content = MapStore::default();
  let (pieces, expected) = view(&mut content);
  let no = volume
    .create_referenced(&mut store, root, "rev", 0o444, pieces, &mut content)
    .unwrap();
  let bytes = volume.to_image(&store, None).unwrap().to_content();
  let image = VolumeImage::from_content(&bytes).unwrap();
  let mut fresh = common::surviving(&store);
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
  assert!(recovered.references_owed());
  assert_eq!(read_whole(&recovered, &fresh, no, &content), expected);
  let mut successor = MapStore {
    objects: content.objects.clone(),
    ..MapStore::default()
  };
  recovered.hold_owed_references(&mut successor).unwrap();
  assert!(!recovered.references_owed());
  assert_eq!(successor.held(), u64::try_from(TEXTS).unwrap());
  recovered.release_references(&mut successor).unwrap();
  assert_eq!(successor.held(), 0);
  recovered.destroy(&mut fresh).unwrap();
}
