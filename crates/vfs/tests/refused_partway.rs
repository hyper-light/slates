//! An operation refused partway leaves a consistent file (§4.5, T-1.1): a write that runs out of room after
//! landing some windows keeps exactly those windows and reports them as a short write, as POSIX `write(2)` does;
//! a truncate whose smaller block cannot be allocated keeps the larger one. Before 2026-10-03 both took the
//! inode's body out, and a refusal after that returned without putting it back: the file lost its content
//! (`docs/bugs/2026-10-03-a-write-or-truncate-refused-partway-dropped-the-files-body.md`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::ids::InodeNo;
use slates_vfs::volume::{Store, StoreConfig, Volume};

/// Shape: the chunk records the write test's store holds, so a write across six windows runs out partway.
const FEW_CHUNKS: usize = 3;

/// A store over the fixture's 16 MiB region with `max_chunks` chunk records.
fn store_with_chunks(max_chunks: usize) -> Store {
  let mut arena = ChunkArena::new(common::PAGE);
  arena
    .add_region(Region::map(common::PAGE * common::REGION_PAGES, common::PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: common::PAGE,
      cache_line: 64,
      max_dirs: 1 << 16,
      max_inodes: 1 << 16,
      max_chunks,
      max_dir_blocks: 1 << 16,
      dir_cutover: 4,
    },
    arena,
    0,
  )
}

/// A file's bytes through the read path, up to `len`.
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

/// T-1.1. Do: in a store of three chunk records, write three windows of `a`, then write six windows of `b` from
/// the start, which runs out of chunk records partway. Expect: a short write of whole windows (some, not all),
/// every written byte reads `b`, the file's size covers them, and nothing before them is lost.
#[test]
fn a_write_refused_partway_keeps_the_windows_it_wrote() {
  let mut store = store_with_chunks(FEW_CHUNKS);
  let mut vol = common::volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let chunk = store.content.chunk_bytes();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, f, 0, &vec![b'a'; 3 * chunk]).unwrap();
  let written = vol
    .write(&mut store, f, 0, &vec![b'b'; 6 * chunk])
    .expect("a write that lands some windows is a short write, not a refusal");
  assert!(
    written > 0 && written < 6 * chunk,
    "a short write: {written}"
  );
  let size = usize::try_from(vol.stat(&store, f).unwrap().size).unwrap();
  assert_eq!(
    size,
    written.max(3 * chunk),
    "the size covers what was written"
  );
  let bytes = read_all(&vol, &store, f, size);
  assert!(
    bytes.get(..written).unwrap().iter().all(|b| *b == b'b'),
    "every written byte reads back"
  );
  assert!(
    bytes.get(written..).unwrap().iter().all(|b| *b == b'a'),
    "what the write did not reach is the file's old content"
  );
}

/// T-1.1. Do: write a file whose open window holds a large block, fill the rest of the arena until it refuses,
/// then truncate the file to one byte (its open window wants a smaller block, which the full arena cannot give).
/// Expect: the truncate succeeds, the file is one byte long, and that byte is the one written.
#[test]
fn a_truncate_in_a_full_arena_keeps_the_file() {
  let mut store = common::store();
  let mut vol = common::volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let chunk = store.content.chunk_bytes();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, f, 0, &vec![b'k'; chunk]).unwrap();
  let filler = vol
    .create_file_no(&mut store, root, "filler", 0o644)
    .unwrap();
  let mut at = 0u64;
  while let Ok(n) = vol.write(&mut store, filler, at, &vec![b'x'; chunk]) {
    if n == 0 {
      break;
    }
    at += u64::try_from(n).unwrap();
  }
  vol.truncate(&mut store, f, 1).unwrap();
  assert_eq!(vol.stat(&store, f).unwrap().size, 1);
  assert_eq!(read_all(&vol, &store, f, 1), b"k");
}

/// T-1.1 (an edit lands whole or not at all). Do: write a file, fill the rest of the arena until it refuses, then
/// edit the file at its start, inserting more bytes than the arena can take. Expect: the edit refused, and the file
/// exactly as it was — its size and every byte (before 2026-10-03 the edit truncated first and could fail after).
#[test]
fn an_edit_without_room_is_refused_with_the_file_unchanged() {
  let mut store = common::store();
  let mut vol = common::volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let chunk = store.content.chunk_bytes();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  let original = vec![b'o'; 2 * chunk];
  vol.write(&mut store, f, 0, &original).unwrap();
  let filler = vol
    .create_file_no(&mut store, root, "filler", 0o644)
    .unwrap();
  let mut at = 0u64;
  while let Ok(n) = vol.write(&mut store, filler, at, &vec![b'x'; chunk]) {
    if n == 0 {
      break;
    }
    at += u64::try_from(n).unwrap();
  }
  assert!(
    vol
      .edit(&mut store, f, 0, 0, &vec![b'e'; 4 * chunk])
      .is_err()
  );
  assert_eq!(
    vol.stat(&store, f).unwrap().size,
    u64::try_from(original.len()).unwrap()
  );
  assert!(read_all(&vol, &store, f, original.len()) == original);
}
