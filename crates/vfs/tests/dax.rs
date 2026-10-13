//! Adversarial tests for the writable-DAX experiment (`slates_vfs::dax`): a hostile guest trying to read or
//! corrupt bytes it is not authorized to, through the shared window over the real content arena. Each test is an
//! attack; the module stays behind them until every vector is closed.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use slates_vfs::dax::{DAX_PAGE, DaxMapper};
use slates_vfs::ids::InodeNo;
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{store, volume};

/// A byte that stands for a secret the user meant to be gone — old content truncated away. If the guest reads it
/// back through a mapping, the truncate did not remove it from the guest's view: a leak.
const SECRET: u8 = 0xAA;

/// Creates a regular file of `fill`×`len` bytes in the root (large enough to live in an arena block, not inline),
/// returns its inode number.
fn file_of(store: &mut Store, volume: &mut Volume, name: &str, fill: u8, len: usize) -> InodeNo {
  let root = volume.root_inode(store).unwrap();
  let no = volume.create_file_no(store, root, name, 0o644).unwrap();
  volume.write(store, no, 0, &vec![fill; len]).unwrap();
  no
}

/// Vector 2 (truncate-kept-block leak). Do: write a whole page of a secret, truncate the file to 100 bytes (the
/// secret past byte 100 is now outside the file), then DAX-map the file and have the guest read the whole page.
/// Expect: bytes 100..4096 read as zero — the truncated-away secret is gone from the guest's view. The guest's
/// page table exposes the *whole* block, and a truncate keeps the block with its old bytes, so a mapping that does
/// not scrub past the file's length hands the guest data the user deleted.
#[test]
fn a_truncated_files_mapping_does_not_leak_the_bytes_past_its_new_length() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let secret = file_of(&mut store, &mut volume, "secret", SECRET, DAX_PAGE);
  volume.truncate(&mut store, secret, 100).unwrap();

  let mut mapper = DaxMapper::new();
  let mapping = mapper
    .setup_read_mapping(&mut store, &volume, secret)
    .unwrap();
  assert_eq!(
    mapping.file_len, 100,
    "the authorized length is the truncated size"
  );

  let page = mapper.guest_read(&store, &mapping, 0, DAX_PAGE).unwrap();
  assert!(
    page[100..].iter().all(|&b| b == 0),
    "the bytes past the file's new length are zero, not the truncated-away secret: {:?}",
    &page[100..120]
  );
}

/// Vector 3 (CoW-break: a writable mapping of a snapshot-shared block). Do: write a page, snapshot it (the live
/// file and the snapshot now share the content block), then DAX-map the live file **writable** and have the guest
/// write it — straight into the shared block, as a DAX guest does with no `Volume::write` and so no copy-up.
/// Expect: the **snapshot** still reads the original bytes. A writable mapping of a shared block lets the guest
/// reach into a version it was never authorized to touch; the fix is copy-up-before-writable-map.
#[test]
fn a_writable_mapping_of_a_snapshot_shared_block_does_not_corrupt_the_snapshot() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  const ORIG: u8 = 0x11;
  const GUEST: u8 = 0x22;
  let f = file_of(&mut store, &mut volume, "f", ORIG, DAX_PAGE);
  let snap = volume.snapshot(&mut store).unwrap();

  let mut mapper = DaxMapper::new();
  let mapping = mapper
    .setup_write_mapping(&mut store, &mut volume, f)
    .unwrap();
  mapper
    .guest_write(&mut store, &mapping, 0, &[GUEST; 200])
    .unwrap();

  // Non-vacuity: the guest's write really landed on the live file's now-private block — the test would pass
  // vacuously if `guest_write` had silently done nothing.
  let mut live = vec![0u8; 200];
  volume.read(&store, f, 0, &mut live).unwrap();
  assert!(
    live.iter().all(|&b| b == GUEST),
    "the guest's write took effect on the live file"
  );

  // ...and the snapshot, which no longer shares that block, is untouched.
  let mut frozen = vec![0u8; DAX_PAGE];
  volume.read_in(&store, snap, f, 0, &mut frozen).unwrap();
  assert!(
    frozen.iter().all(|&b| b == ORIG),
    "the snapshot still reads its original bytes, not the guest's write: {:?}",
    &frozen[..8]
  );
}

/// Vector 4 (writing past the authorized length). Do: map a 100-byte file writable (its block is a whole page),
/// then have the guest write at offset 100 — into the page's scrubbed tail, past the file. Expect: refused. The
/// mapping authorizes only the file's current bytes; growing it needs a fresh quota and lease check the guest
/// cannot make itself, so a write past the length must be refused — not silently kept, which would let the guest
/// grow the file (and exceed its pre-charged quota) straight through the window.
#[test]
fn a_guest_write_past_the_authorized_length_is_refused() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  const G: u8 = 0x33;
  // A block-backed file of length 100: write a page, truncate to 100, so the block is a page and the length 100.
  let f = file_of(&mut store, &mut volume, "f", 0x44, DAX_PAGE);
  volume.truncate(&mut store, f, 100).unwrap();

  let mut mapper = DaxMapper::new();
  let mapping = mapper
    .setup_write_mapping(&mut store, &mut volume, f)
    .unwrap();
  assert_eq!(mapping.file_len, 100, "the authorized length is 100");

  assert!(
    mapper
      .guest_write(&mut store, &mapping, 0, &[G; 100])
      .is_ok(),
    "a write within the file's length is allowed"
  );
  assert!(
    mapper
      .guest_write(&mut store, &mapping, 100, &[G; 16])
      .is_err(),
    "a write past the file's length is refused, not kept in the block's tail"
  );
  assert!(
    mapper
      .guest_write(&mut store, &mapping, 95, &[G; 16])
      .is_err(),
    "a write straddling the file's end is refused whole"
  );
}
