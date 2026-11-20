//! The shard's content store as a referenced file's seam (A-645 P3 ruling 2; A-64, A-727; `slates_vfs::referenced`):
//! a hook's view reads its texts from the store by identity, and holds them for as long as the view's volume lives.
//!
//! [`Lent`] answers a read: the run of an object's bytes at an offset is the store's window that holds it
//! (`ContentStore::window_at`, which resolves the identity to the root the store cut the object into), cut at the
//! offset and at the length the piece shows. A window the storage lends is answered borrowed; one it could only copy
//! out (a sealed chunk read outside a step's lend, A-727) is answered owned, which the volume counts in
//! `bytes_copied`. [`Holding`] holds and releases an identity through `ContentStore::hold` and `release`, which resolve
//! it too, so a view names an object by its identity alone, whatever window this node cut it at (the identity rule,
//! 9010c5630).

use std::borrow::Cow;

use slates_vcs::{ContentId, ContentStore};
use slates_vfs::VfsError;
use slates_vfs::referenced::{Identity, ReferencedContent, ReferencedHolds};

/// The content store lent shared, for a referenced read.
pub(crate) struct Lent<'s>(pub(crate) &'s ContentStore);

/// The content store lent to change, for a referenced file's holds.
pub(crate) struct Holding<'s>(pub(crate) &'s mut ContentStore);

impl ReferencedContent for Lent<'_> {
  /// The run of `id` from `offset` the content store lends (`ContentStore::run_at`: its window holding `offset`, the
  /// object's length checked as the store can; a chunk in the clear lent in place, nothing copied).
  fn run_at(&self, id: &Identity, len: u64, offset: u64) -> Result<Cow<'_, [u8]>, VfsError> {
    self
      .0
      .run_at(&ContentId(*id), len, offset)
      .filter(|run| !run.is_empty())
      .ok_or(VfsError::ReferenceUnresolved)
  }
}

impl ReferencedHolds for Holding<'_> {
  fn hold(&mut self, id: &Identity) -> Result<(), VfsError> {
    self
      .0
      .hold(&ContentId(*id))
      .map_err(|_| VfsError::ReferenceUnresolved)
  }

  fn release(&mut self, id: &Identity) -> Result<(), VfsError> {
    self
      .0
      .release(&ContentId(*id))
      .map(|_| ())
      .map_err(|_| VfsError::ReferenceUnresolved)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_mem::arena::ChunkArena;
  use slates_mem::region::Region;
  use slates_vfs::NameEquivalence;
  use slates_vfs::clock::StepClock;
  use slates_vfs::quota::Quota;
  use slates_vfs::referenced::{Piece, ReferencedCounts};
  use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

  /// Shape: the store's window, small so every text spans several.
  const WINDOW: usize = 64;

  /// Shape: the texts the view references.
  const TEXTS: usize = 24;

  /// Shape: a store's budget no test comes near.
  const ROOM: u64 = 1 << 24;

  /// Shape: the bytes a reader asks for at once: prime, so reads start and end inside windows.
  const READ_BYTES: usize = 509;

  /// Text `n`: bytes no other text shares, from one byte to a little over five windows.
  fn text(n: usize) -> Vec<u8> {
    let len = 1 + (n * 53) % (5 * WINDOW + 7);
    (0..len)
      .map(|at| u8::try_from((n * 131 + at * 7) % 251).unwrap())
      .collect()
  }

  /// A-645 P3 ruling 2, the server's seam. Do: put [`TEXTS`] objects into a content store, some past one window; make a
  /// referenced file of them, each after a made header, through [`Holding`]; read it whole through [`Lent`]; release.
  /// Expect each object held once more while the file's volume holds it and as before after the release; the bytes the
  /// headers and objects in order; `bytes_read` the file's length and `bytes_copied` zero.
  #[test]
  fn a_referenced_file_reads_the_content_stores_objects_lent_and_holds_them() {
    let mut content = ContentStore::new(ROOM, WINDOW).unwrap();
    let mut pieces = Vec::new();
    let mut expected = Vec::new();
    let mut ids = Vec::new();
    for n in 0..TEXTS {
      let header = format!("DELTA {n}\n").into_bytes();
      expected.extend_from_slice(&header);
      pieces.push(Piece::Made(header.into_boxed_slice()));
      let bytes = text(n);
      let id = content.put_object(&bytes).unwrap();
      expected.extend_from_slice(&bytes);
      pieces.push(Piece::Content {
        id: id.0,
        len: u64::try_from(bytes.len()).unwrap(),
      });
      ids.push(id);
    }
    assert!(
      ids.iter().any(|id| content.root_of(id) != *id),
      "some object spans windows"
    );
    let before: Vec<u64> = ids
      .iter()
      .map(|id| content.holders(&content.root_of(id)))
      .collect();
    let mut store = vfs_store();
    let mut volume = Volume::create(
      &mut store,
      VolumeConfig {
        prefix: 7,
        names: NameEquivalence::Fold,
        quota: Quota::Bounded { limit: ROOM },
        journal_bytes: 1 << 20,
        clock: Box::new(StepClock::new(0, 1_000)),
      },
    )
    .unwrap();
    let root = volume.root_inode(&store).unwrap();
    let no = volume
      .create_referenced(
        &mut store,
        root,
        "rev",
        0o444,
        pieces,
        &mut Holding(&mut content),
      )
      .unwrap();
    for (id, was) in ids.iter().zip(&before) {
      assert_eq!(content.holders(&content.root_of(id)), was + 1);
    }
    let mut out = Vec::new();
    let mut buf = vec![0u8; READ_BYTES];
    loop {
      let offset = u64::try_from(out.len()).unwrap();
      let read = volume
        .read_referenced(&store, no, offset, &mut buf, &Lent(&content))
        .unwrap();
      if read == 0 {
        break;
      }
      out.extend_from_slice(&buf[..read]);
    }
    assert_eq!(out, expected);
    assert_eq!(
      volume.referenced_counts(),
      ReferencedCounts {
        bytes_read: u64::try_from(expected.len()).unwrap(),
        bytes_copied: 0,
      }
    );
    volume
      .release_references(&mut Holding(&mut content))
      .unwrap();
    for (id, was) in ids.iter().zip(&before) {
      assert_eq!(content.holders(&content.root_of(id)), *was);
    }
    volume.destroy(&mut store).unwrap();
  }

  /// A-645 P3 ruling 2. Do: read a one-window object under a length one byte long and one byte short, and at its length;
  /// a tree object under a length of one window and of one window more; an identity the store does not hold. Expect
  /// each read refused `ReferenceUnresolved`, never another object's bytes, and the unknown identity's hold refused.
  #[test]
  fn a_run_at_a_wrong_length_or_of_no_object_is_refused() {
    let mut content = ContentStore::new(ROOM, WINDOW).unwrap();
    let short = text(7);
    let long = text(9);
    assert!(short.len() <= WINDOW && long.len() > WINDOW);
    let short_id = content.put_object(&short).unwrap();
    let long_id = content.put_object(&long).unwrap();
    let short_len = u64::try_from(short.len()).unwrap();
    let long_len = u64::try_from(long.len()).unwrap();
    let window = u64::try_from(WINDOW).unwrap();
    let lent = Lent(&content);
    for (id, len, offset) in [
      (short_id, short_len + 1, 0),
      (short_id, short_len - 1, 0),
      (short_id, short_len, short_len),
      (long_id, window, 0),
      (long_id, long_len + window, long_len),
    ] {
      assert_eq!(
        lent.run_at(&id.0, len, offset),
        Err(VfsError::ReferenceUnresolved),
        "{len} at {offset}"
      );
    }
    assert_eq!(
      lent.run_at(&[0xee; 32], 1, 0),
      Err(VfsError::ReferenceUnresolved)
    );
    assert_eq!(
      Holding(&mut content).hold(&[0xee; 32]),
      Err(VfsError::ReferenceUnresolved)
    );
  }

  /// A vfs store with one small arena region.
  fn vfs_store() -> Store {
    let page = 4096;
    let mut arena = ChunkArena::new(page);
    arena
      .add_region(Region::map(page * 256, page, false).unwrap())
      .unwrap();
    Store::new(
      &StoreConfig {
        page,
        cache_line: 64,
        max_dirs: 64,
        max_inodes: 64,
        max_chunks: 64,
        max_dir_blocks: 64,
        dir_cutover: 4,
      },
      arena,
      0,
    )
  }
}
