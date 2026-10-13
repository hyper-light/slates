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
use common::{clone_config, store, volume};

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

  let page = mapper
    .guest_read(&store, &volume, &mapping, 0, DAX_PAGE)
    .unwrap();
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
    .guest_write(&mut store, &volume, &mapping, 0, &[GUEST; 200])
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
      .guest_write(&mut store, &volume, &mapping, 0, &[G; 100])
      .is_ok(),
    "a write within the file's length is allowed"
  );
  assert!(
    mapper
      .guest_write(&mut store, &volume, &mapping, 100, &[G; 16])
      .is_err(),
    "a write past the file's length is refused, not kept in the block's tail"
  );
  assert!(
    mapper
      .guest_write(&mut store, &volume, &mapping, 95, &[G; 16])
      .is_err(),
    "a write straddling the file's end is refused whole"
  );
}

/// Vector 5 (use-after-free: a mapping outliving its file). Do: map a file read-only, unlink it (its block is
/// freed — and zeroed, A-99), then create a different file that reuses the block and fill it with a new secret.
/// Expect: the stale mapping does **not** read the new file's secret. Without pinning the mapper holds a raw block
/// the arena then hands to the next file, so the old mapping reads a neighbour it was never authorized to see —
/// and a writable one would corrupt it. The fix is to pin the block for the mapping's life.
#[test]
fn a_mapping_does_not_outlive_its_file_into_a_neighbour_that_reused_the_block() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  const SECRET_G: u8 = 0x55;
  let f = file_of(&mut store, &mut volume, "f", 0x44, DAX_PAGE);
  let root = volume.root_inode(&store).unwrap();

  let mut mapper = DaxMapper::new();
  let m_f = mapper.setup_read_mapping(&mut store, &volume, f).unwrap();
  let freed = m_f.block;

  volume.unlink_no(&mut store, root, "f").unwrap();

  // A different file reuses the freed block.
  let g = file_of(&mut store, &mut volume, "g", SECRET_G, DAX_PAGE);
  let m_g = mapper.setup_read_mapping(&mut store, &volume, g).unwrap();

  // The attack only bites if the *physical* block was reused — the arena bumps the generational incarnation on
  // reuse, so the Extents are not equal, but `arena.bytes` slices by region+offset and ignores the incarnation,
  // so a stale mapping still reads the new block's bytes. Assert the physical reuse so a non-reuse cannot pass
  // the test vacuously.
  assert!(
    m_g.block.region() == freed.region() && m_g.block.offset() == freed.offset(),
    "the new file reused the freed physical block (the use-after-free precondition)"
  );

  // The still-current check makes the stale mapping a typed miss: the arena bumped the block's incarnation on
  // reuse, so it no longer equals the mapping's extent, and the read is refused rather than serving the neighbour.
  assert!(
    mapper
      .guest_read(&store, &volume, &m_f, 0, DAX_PAGE)
      .is_err(),
    "the stale mapping is refused, not served the neighbour that reused the block"
  );
}

/// HORROR 1 (snapshot AFTER a writable map defeats the copy-up). Do: map a file writable (copy-up makes its block
/// private), THEN snapshot it (the snapshot shares the just-copied-up block), THEN have the guest write. Expect:
/// the snapshot is NOT corrupted. The copy-up at setup does not help — a snapshot taken afterward re-shares the
/// block, and `still_current` cannot see it (the block identity is unchanged, only its sharing). The fix must copy
/// up around writable mappings when a snapshot is taken.
#[test]
fn a_snapshot_taken_after_a_writable_map_is_not_corrupted_by_a_later_guest_write() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  const ORIG: u8 = 0x11;
  const EVIL: u8 = 0xEE;
  let f = file_of(&mut store, &mut volume, "f", ORIG, DAX_PAGE);

  let mut mapper = DaxMapper::new();
  let m = mapper
    .setup_write_mapping(&mut store, &mut volume, f)
    .unwrap();
  // The snapshot is taken AFTER the mapping, so it would share the copied-up block — the snapshot path must
  // re-assert the writable mappings private (copy them up again, re-pointing the mapping at a fresh block).
  let snap = volume.snapshot(&mut store).unwrap();
  mapper.reassert_private(&mut store, &mut volume).unwrap();
  // The guest's held mapping is now stale (its block moved), so this write is a typed miss; a guest that re-maps
  // writes a fresh private block, never the snapshot's.
  let _ = mapper.guest_write(&mut store, &volume, &m, 0, &[EVIL; 100]);

  let mut frozen = vec![0u8; DAX_PAGE];
  volume.read_in(&store, snap, f, 0, &mut frozen).unwrap();
  assert!(
    frozen[..100].iter().all(|&b| b == ORIG),
    "the snapshot taken after the writable map is not corrupted by the guest's later write: {:?}",
    &frozen[..8]
  );
}

/// HORROR 2 (the block moves under the mapping). Do: map a file writable, then grow it through the VFS so its
/// block reallocates, then use the old mapping. Expect: refused — the file's block moved, so the mapping is stale.
#[test]
fn a_mapping_is_refused_after_the_files_block_moves() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  volume.truncate(&mut store, f, 100).unwrap();

  let mut mapper = DaxMapper::new();
  let m = mapper
    .setup_write_mapping(&mut store, &mut volume, f)
    .unwrap();
  // Grow F far past its block, forcing a realloc to a new, larger block.
  volume
    .write(&mut store, f, 0, &vec![0x22; DAX_PAGE * 4])
    .unwrap();

  assert!(
    mapper
      .guest_write(&mut store, &volume, &m, 0, &[0x33; 50])
      .is_err(),
    "a write through a mapping whose block moved is refused"
  );
  assert!(
    mapper.guest_read(&store, &volume, &m, 0, 50).is_err(),
    "a read through a mapping whose block moved is refused"
  );
}

/// HORROR 3 (arithmetic abuse). Do: colossal and overflowing offsets/lengths on a valid mapping. Expect: no
/// panic, no out-of-bounds — a read comes back zero (nothing mapped there), an overflowing write is refused.
#[test]
fn colossal_and_overflowing_offsets_do_not_panic_or_read_past_the_block() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);

  let mut mapper = DaxMapper::new();
  let m = mapper
    .setup_write_mapping(&mut store, &mut volume, f)
    .unwrap();

  assert!(
    mapper
      .guest_read(&store, &volume, &m, usize::MAX - 10, 100)
      .is_err(),
    "a read at a colossal offset is refused, not served from arena memory past the block"
  );
  assert!(
    mapper
      .guest_write(&mut store, &volume, &m, usize::MAX, &[0u8; 10])
      .is_err(),
    "a write whose offset+length overflows is refused, not wrapped"
  );
}

/// HORROR 6 (unbounded read — pile a buffer to crash the daemon). Do: ask a valid mapping for a read far past the
/// mapped page, then for a colossal length. Expect: refused. `guest_read` allocating a guest-chosen length is an
/// unbounded allocation (CLAUDE.md #8): a read of `usize::MAX` makes the daemon try to allocate exabytes and abort.
/// The read must be bounded to the mapped page span and refused before a single byte is allocated.
#[test]
fn an_unbounded_read_length_is_refused_not_piled_into_a_crashing_allocation() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);

  let mut mapper = DaxMapper::new();
  let m = mapper.setup_read_mapping(&mut store, &volume, f).unwrap();

  assert!(
    mapper
      .guest_read(&store, &volume, &m, 0, m.page_span() + DAX_PAGE)
      .is_err(),
    "a read past the mapped page span is refused, not allocated and zero-filled"
  );
  assert!(
    mapper
      .guest_read(&store, &volume, &m, 0, usize::MAX)
      .is_err(),
    "a colossal read length is refused before any allocation — no exabyte pile, no abort"
  );
}

/// HORROR 4 (write through a read-only mapping). Do: a read-only mapping; the guest attempts a write. Expect:
/// refused — a read-only mapping is never a write path.
#[test]
fn a_write_through_a_read_only_mapping_is_refused() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);

  let mut mapper = DaxMapper::new();
  let m = mapper.setup_read_mapping(&mut store, &volume, f).unwrap();
  assert!(
    mapper
      .guest_write(&mut store, &volume, &m, 0, &[0x99; 10])
      .is_err(),
    "a write through a read-only mapping is refused"
  );
}

/// HORROR 5 (map a non-regular file). Do: map the root directory as if it were a file. Expect: refused — only a
/// regular file's content block may be mapped; a directory's internal structure must never reach a guest.
#[test]
fn mapping_a_directory_is_refused() {
  let mut store = store();
  let volume = volume(&mut store, 1 << 20);
  let root = volume.root_inode(&store).unwrap();

  let mut mapper = DaxMapper::new();
  assert!(
    mapper
      .setup_read_mapping(&mut store, &volume, root)
      .is_err(),
    "mapping a directory is refused"
  );
}

/// HORROR 7 (burst / churn / ramp-up). Do: hundreds of rounds of create → map writable → write a round-specific
/// secret → unlink, reusing the arena block round after round, keeping every past mapping. After each round,
/// hammer EVERY past mapping with a read. Expect: not one past mapping ever reads content under churn — each is a
/// typed miss — and nothing panics. An incarnation collision or a free-list slip would surface as a cross-round
/// read returning a later round's secret.
#[test]
fn a_burst_of_churned_mappings_never_leaks_across_rounds() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  let mut mapper = DaxMapper::new();
  let mut past: Vec<slates_vfs::dax::Mapping> = Vec::new();

  for round in 0..400u32 {
    let secret = u8::try_from((round % 254) + 1).unwrap_or(1); // 1..=254, never zero
    let name = format!("f{round}");
    let f = volume
      .create_file_no(&mut store, root, &name, 0o644)
      .unwrap();
    volume
      .write(&mut store, f, 0, &vec![0u8; DAX_PAGE])
      .unwrap();
    volume.truncate(&mut store, f, 200).unwrap();
    let m = mapper
      .setup_write_mapping(&mut store, &mut volume, f)
      .unwrap();
    mapper
      .guest_write(&mut store, &volume, &m, 0, &[secret; 200])
      .unwrap();

    // Every past mapping's file is gone and its block churned: each read must be a typed miss (or, if ever
    // served, only zeros — never a round's secret).
    for old in &past {
      if let Ok(bytes) = mapper.guest_read(&store, &volume, old, 0, DAX_PAGE) {
        assert!(
          bytes.iter().all(|&b| b == 0),
          "a stale mapping from a prior round read content under churn — a cross-round leak"
        );
      }
    }
    past.push(m);
    volume.unlink_no(&mut store, root, &name).unwrap();
  }
}

/// THE ESCAPE (the one re-validation cannot close). A real DAX guest reads the mapped page in hardware and never
/// calls the daemon, so the daemon's per-access `still_current` check does not run on the guest's reads. Do: map a
/// file, unlink it (its block is freed), let a DIFFERENT file reuse that block, then read through the OLD mapping
/// the way hardware does (`guest_read_raw`, no daemon check). Expect (today, without pinning): the read returns the
/// other file's bytes — a genuine escape across the authorization boundary. The daemon-mediated read IS refused
/// (`still_current`), but the guest never takes that path. This is why writable DAX MUST pin the block for the
/// mapping's life and revoke the page table before any free — the daemon cannot mediate the guest's hardware reads,
/// so re-validation is not a defense. (When pinning is built, the block is never reused and this read is the file's
/// own zeros; until then this documents the open escape.)
#[test]
fn the_hardware_read_path_escapes_into_a_reused_block_that_revalidation_cannot_guard() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  const NEIGHBOUR: u8 = 0x5A;
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let root = volume.root_inode(&store).unwrap();

  let mut mapper = DaxMapper::new();
  let m = mapper
    .setup_write_mapping(&mut store, &mut volume, f)
    .unwrap();
  let mapped = m.block;

  volume.unlink_no(&mut store, root, "f").unwrap();
  // A different file reuses the freed physical block, carrying the neighbour's secret.
  let g = file_of(&mut store, &mut volume, "g", NEIGHBOUR, DAX_PAGE);
  let gm = mapper.setup_read_mapping(&mut store, &volume, g).unwrap();
  assert!(
    gm.block.region() == mapped.region() && gm.block.offset() == mapped.offset(),
    "the neighbour reused the freed physical block (the escape precondition)"
  );

  // The daemon-mediated read is refused — but no real DAX guest takes that path.
  assert!(
    mapper.guest_read(&store, &volume, &m, 0, DAX_PAGE).is_err(),
    "the daemon-mediated read is refused by still_current"
  );
  // The hardware path — what the guest actually uses — reads the neighbour's bytes. The escape.
  let raw = mapper.guest_read_raw(&store, &m, 0, DAX_PAGE);
  assert!(
    raw.contains(&NEIGHBOUR),
    "the hardware read path escaped into the neighbour's reused block (only pinning + revocation can close this)"
  );
}

/// ESCAPE 2 (hardware write into a reused block corrupts a neighbour). Map a file writable, unlink it, let another
/// file reuse the block, then WRITE through the old mapping the hardware way (`guest_write_raw`, no daemon check).
/// Expect: the neighbour's bytes are overwritten — cross-authorization corruption the daemon never sees. Only
/// pinning (never reuse a mapped block) and revocation close it.
#[test]
fn the_hardware_write_path_corrupts_a_neighbour_that_reused_the_block() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let root = volume.root_inode(&store).unwrap();

  let mut mapper = DaxMapper::new();
  let m = mapper
    .setup_write_mapping(&mut store, &mut volume, f)
    .unwrap();
  let mapped = m.block;

  volume.unlink_no(&mut store, root, "f").unwrap();
  let g = file_of(&mut store, &mut volume, "g", 0x5A, DAX_PAGE);
  let gm = mapper.setup_read_mapping(&mut store, &volume, g).unwrap();
  assert!(
    gm.block.region() == mapped.region() && gm.block.offset() == mapped.offset(),
    "the neighbour reused the freed physical block"
  );

  mapper.guest_write_raw(&mut store, &m, 0, &[0xEE; 100]); // hardware write into the reused block
  let mut gbytes = vec![0u8; 100];
  volume.read(&store, g, 0, &mut gbytes).unwrap();
  assert!(
    gbytes.contains(&0xEE),
    "the hardware write corrupted the neighbour's reused block"
  );
}

/// ESCAPE 3 (hardware write past the authorized length). The daemon bounds `guest_write` to `file_len`, but the
/// hardware path writes the whole mapped page. Map a 100-byte file writable (page block), write at offset 100 the
/// hardware way. Expect: it lands past the file — the `file_len` bound is fiction for the page the guest holds.
#[test]
fn the_hardware_write_path_writes_past_the_authorized_length() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  volume.truncate(&mut store, f, 100).unwrap();

  let mut mapper = DaxMapper::new();
  let m = mapper
    .setup_write_mapping(&mut store, &mut volume, f)
    .unwrap();
  assert_eq!(m.file_len, 100, "the authorized length is 100");

  // The daemon refuses a write past 100...
  assert!(
    mapper
      .guest_write(&mut store, &volume, &m, 100, &[0xEE; 50])
      .is_err(),
    "the daemon-side bound refuses a write past the length"
  );
  // ...but the hardware path writes it anyway, into the page tail.
  mapper.guest_write_raw(&mut store, &m, 100, &[0xEE; 50]);
  let tail = mapper.guest_read_raw(&store, &m, 100, 50);
  assert!(
    tail.contains(&0xEE),
    "the hardware write escaped past the authorized length into the page tail"
  );
}

/// ESCAPE 4 (`reassert_private` does not stop the hardware write). Even after the snapshot path re-copies-up a
/// writable mapping, the GUEST still holds the OLD block in its page table. A hardware write hits that old block —
/// which the snapshot shares — and corrupts the snapshot. `reassert_private` (daemon-side) is fiction for the
/// hardware path; only REVOKING the mapping (tearing the page table down) before the snapshot shares the block
/// closes it.
#[test]
fn the_hardware_write_path_corrupts_a_snapshot_despite_reassert_private() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  const ORIG: u8 = 0x11;
  let f = file_of(&mut store, &mut volume, "f", ORIG, DAX_PAGE);

  let mut mapper = DaxMapper::new();
  let m = mapper
    .setup_write_mapping(&mut store, &mut volume, f)
    .unwrap();
  let snap = volume.snapshot(&mut store).unwrap();
  mapper.reassert_private(&mut store, &mut volume).unwrap();

  // The guest's held mapping still points at the block the snapshot now shares; the hardware write hits it.
  mapper.guest_write_raw(&mut store, &m, 0, &[0xEE; 100]);
  let mut frozen = vec![0u8; 100];
  volume.read_in(&store, snap, f, 0, &mut frozen).unwrap();
  assert!(
    frozen.contains(&0xEE),
    "the hardware write corrupted the snapshot despite reassert_private — only revocation closes it"
  );
}

/// THE FAILURE — run it and watch isolation break. This asserts the security property slates MUST uphold: a guest,
/// through a DAX mapping, may touch only its own authorized content, never another file. It **fails** — via the
/// hardware path a stale mapping corrupts a neighbour that reused the block, and no daemon-side check runs on the
/// guest's own page accesses. It can pass only once the **pinning + revocation gate** exists (a mapped block is
/// never freed, reused, or shared, and its page table is torn down before any of those), the gate this whole
/// experiment proves mandatory. `#[ignore]` so the suite stays green; run it to see the red:
/// `cargo test -p slates-vfs --test dax -- --ignored writable_dax_upholds_cross_file_isolation`.
#[test]
#[ignore = "DEMONSTRATION: fails on purpose — writable DAX breaks cross-file isolation on the hardware path until pinning + revocation exist"]
fn writable_dax_upholds_cross_file_isolation() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  const NEIGHBOUR: u8 = 0x5A;
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let root = volume.root_inode(&store).unwrap();

  let mut mapper = DaxMapper::new();
  let m = mapper
    .setup_write_mapping(&mut store, &mut volume, f)
    .unwrap();

  // The guest's file is unlinked and a different file reuses its block.
  volume.unlink_no(&mut store, root, "f").unwrap();
  let g = file_of(&mut store, &mut volume, "g", NEIGHBOUR, DAX_PAGE);

  // The guest writes its OWN (now-freed) mapping the hardware way — no daemon check runs.
  mapper.guest_write_raw(&mut store, &m, 0, &[0xEE; 100]);

  // THE GUARANTEE: the neighbour g is untouched. It is not — a guest corrupted another file through DAX.
  let mut gbytes = vec![0u8; 100];
  volume.read(&store, g, 0, &mut gbytes).unwrap();
  assert!(
    gbytes.iter().all(|&b| b == NEIGHBOUR),
    "ISOLATION BROKEN: a DAX guest corrupted another file through the hardware path (got {:?}); only pinning + revocation uphold this",
    &gbytes[..8]
  );
}

// ───────────────────────────────────────────────────────────────────────────────────────────────────────────────
// FAILING GUARANTEE SUITE (`cargo test -p slates-vfs --test dax -- --ignored`). Each test asserts a security
// property slates MUST uphold for writable DAX and FAILS today, because the guest's hardware path (`guest_*_raw`)
// obeys no daemon-side guard. Together they are the specification of what the pinning + revocation gate must close:
// a mapped block is never freed, reused, or shared, and its guest page table is torn down and quiesced before any
// of those. Each goes green only when that gate exists. One test per attack class: exfiltration, injection, volume
// escape, TOCTOU, race / lost update, and teardown (deadlock-adjacent). Marked `#[ignore]` so the suite stays
// green; they are demonstrations, not regressions.
// ───────────────────────────────────────────────────────────────────────────────────────────────────────────────

/// EXFILTRATION (confidentiality, same volume). Guarantee: a guest reads only its own authorized content, never a
/// neighbour's bytes. Fails: map a file read-only, free it, let a different file reuse the block with a secret, and
/// the guest reads that secret through the stale mapping the hardware way — the arena slices by region+offset and
/// ignores the generational incarnation, so `guest_read_raw` hands the guest a file it never opened.
#[test]
#[ignore = "DEMONSTRATION: fails on purpose — a writable-DAX guest exfiltrates a neighbour's secret on the hardware path until pinning + revocation exist"]
fn writable_dax_must_not_exfiltrate_a_neighbours_secret() {
  const NEIGHBOUR_SECRET: u8 = 0xAB;
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let root = volume.root_inode(&store).unwrap();

  let mut mapper = DaxMapper::new();
  let m = mapper.setup_read_mapping(&mut store, &volume, f).unwrap();
  let freed = m.block;

  volume.unlink_no(&mut store, root, "f").unwrap();
  let g = file_of(&mut store, &mut volume, "g", NEIGHBOUR_SECRET, DAX_PAGE);
  let gblock = mapper.setup_read_mapping(&mut store, &volume, g).unwrap().block;
  assert!(
    gblock.region() == freed.region() && gblock.offset() == freed.offset(),
    "the neighbour reused the freed physical block (the exfiltration precondition)"
  );

  // The hardware read of the guest's OWN stale mapping returns the neighbour's secret.
  let raw = mapper.guest_read_raw(&store, &m, 0, DAX_PAGE);
  assert!(
    !raw.contains(&NEIGHBOUR_SECRET),
    "EXFILTRATION: a DAX guest read another file's secret through the hardware path (0x{NEIGHBOUR_SECRET:02X} present); only pinning (never reuse a mapped block) upholds confidentiality"
  );
}

/// VOLUME ESCAPE — INJECTION (integrity, cross tenant). Guarantee: a guest in one volume can never alter a file in
/// another volume. Fails: the shard's content arena is shared across every volume it hosts, so an attacker volume's
/// freed block is handed to a victim volume, and the attacker's stale hardware write corrupts the other tenant's
/// file. This is the escape out of the volume boundary — the daemon never sees the write.
#[test]
#[ignore = "DEMONSTRATION: fails on purpose — a writable-DAX guest escapes its volume and corrupts another tenant on the hardware path until pinning + revocation exist"]
fn writable_dax_must_not_let_a_guest_escape_into_another_volume() {
  const POISON: u8 = 0xEE;
  const VICTIM: u8 = 0x5A;
  let mut store = store();
  let mut attacker = volume(&mut store, 1 << 20); // volume A (prefix 7)
  let mut victim = Volume::create(&mut store, clone_config(9)).unwrap(); // volume B (prefix 9), same shard arena

  let fa = file_of(&mut store, &mut attacker, "fa", 0x11, DAX_PAGE);
  let root_a = attacker.root_inode(&store).unwrap();
  let mut mapper = DaxMapper::new();
  let m = mapper
    .setup_write_mapping(&mut store, &mut attacker, fa)
    .unwrap();
  let freed = m.block;
  attacker.unlink_no(&mut store, root_a, "fa").unwrap();

  // The OTHER volume allocates a file from the same arena and reuses the freed block.
  let fb = file_of(&mut store, &mut victim, "fb", VICTIM, DAX_PAGE);
  let vblock = mapper
    .setup_read_mapping(&mut store, &victim, fb)
    .unwrap()
    .block;
  assert!(
    vblock.region() == freed.region() && vblock.offset() == freed.offset(),
    "the victim volume reused the attacker's freed physical block (the escape precondition)"
  );

  mapper.guest_write_raw(&mut store, &m, 0, &[POISON; 100]);
  let mut got = vec![0u8; 100];
  victim.read(&store, fb, 0, &mut got).unwrap();
  assert!(
    got.iter().all(|&b| b == VICTIM),
    "VOLUME ESCAPE: a DAX guest in one volume corrupted a file in ANOTHER volume on the same shard (got {:?}); the arena is shared, so only pinning + revocation confine a guest to its own volume",
    &got[..8]
  );
}

/// VOLUME ESCAPE — EXFILTRATION (confidentiality, cross tenant). Guarantee: a guest in one volume can never read a
/// file in another volume. Fails: the attacker maps and frees a file; the victim volume reuses the block for a
/// secret; the attacker reads it the hardware way — a cross-tenant confidentiality breach straight through the
/// shared arena.
#[test]
#[ignore = "DEMONSTRATION: fails on purpose — a writable-DAX guest reads across the volume boundary on the hardware path until pinning + revocation exist"]
fn writable_dax_must_not_read_across_a_volume_boundary() {
  const SECRET_B: u8 = 0xAB;
  let mut store = store();
  let mut attacker = volume(&mut store, 1 << 20);
  let mut victim = Volume::create(&mut store, clone_config(9)).unwrap();

  let fa = file_of(&mut store, &mut attacker, "fa", 0x11, DAX_PAGE);
  let root_a = attacker.root_inode(&store).unwrap();
  let mut mapper = DaxMapper::new();
  let m = mapper.setup_read_mapping(&mut store, &attacker, fa).unwrap();
  let freed = m.block;
  attacker.unlink_no(&mut store, root_a, "fa").unwrap();

  let fb = file_of(&mut store, &mut victim, "fb", SECRET_B, DAX_PAGE);
  let vblock = mapper
    .setup_read_mapping(&mut store, &victim, fb)
    .unwrap()
    .block;
  assert!(
    vblock.region() == freed.region() && vblock.offset() == freed.offset(),
    "the victim volume reused the attacker's freed physical block (the exfiltration precondition)"
  );

  let raw = mapper.guest_read_raw(&store, &m, 0, DAX_PAGE);
  assert!(
    !raw.contains(&SECRET_B),
    "CROSS-VOLUME EXFILTRATION: a DAX guest read another tenant's secret through the hardware path (0x{SECRET_B:02X} present); only pinning + revocation confine a guest to its own volume"
  );
}

/// TOCTOU (time-of-check to time-of-use). Guarantee: the check that authorizes an access governs the access. Fails:
/// the daemon's `still_current` check and the guest's hardware use are on DIFFERENT code paths with an unbounded
/// window between map and access. Here the check CAN see the staleness (the daemon-mediated write is refused), yet
/// the hardware use is not gated by it and corrupts the reuser anyway. Re-checking at the daemon cannot close this —
/// the guest never calls the daemon; only pinning the block and revoking the page table can.
#[test]
#[ignore = "DEMONSTRATION: fails on purpose — the daemon's check and the guest's hardware use are decoupled (TOCTOU) until pinning + revocation exist"]
fn writable_dax_check_must_govern_the_hardware_use_not_a_different_path() {
  const NEIGHBOUR: u8 = 0x5A;
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let root = volume.root_inode(&store).unwrap();

  let mut mapper = DaxMapper::new();
  let m = mapper
    .setup_write_mapping(&mut store, &mut volume, f)
    .unwrap(); // check OK at map time (T0)

  // The world changes after the check: the file is freed and a neighbour reuses the block.
  volume.unlink_no(&mut store, root, "f").unwrap();
  let g = file_of(&mut store, &mut volume, "g", NEIGHBOUR, DAX_PAGE);

  // TIME OF CHECK: the daemon-mediated path refuses — `still_current` CAN detect the staleness.
  assert!(
    mapper
      .guest_write(&mut store, &volume, &m, 0, &[0xEE; 50])
      .is_err(),
    "the daemon's still_current check (TOC) detects the stale mapping"
  );
  // TIME OF USE: the guest's hardware path is gated by nothing and corrupts the neighbour anyway.
  mapper.guest_write_raw(&mut store, &m, 0, &[0xEE; 50]);

  let mut got = vec![0u8; 50];
  volume.read(&store, g, 0, &mut got).unwrap();
  assert!(
    got.iter().all(|&b| b == NEIGHBOUR),
    "TOCTOU: the check refused the access but the hardware use was not bound by that verdict and corrupted the neighbour (got {:?}); the check and the use must be the same gate — pinning + revocation, not re-checking",
    &got[..8]
  );
}

/// RACE / LOST UPDATE. Guarantee: a guest's write to its own file is durably applied to that file or cleanly
/// refused — never silently discarded or redirected. Fails: a concurrent VFS writer grows the file, moving its
/// content block and freeing the old one WITHOUT revoking the guest. The guest's in-flight hardware write lands on
/// the stale block, so its write vanishes from its own file (and, if the old block was reused, lands in a stranger).
/// The move must revoke and quiesce the guest first.
#[test]
#[ignore = "DEMONSTRATION: fails on purpose — a block move under a live mapping loses the guest's write (race) until pinning + revocation exist"]
fn writable_dax_a_block_move_must_not_lose_a_guest_write() {
  const GUEST: u8 = 0xEE;
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);

  let mut mapper = DaxMapper::new();
  let m = mapper
    .setup_write_mapping(&mut store, &mut volume, f)
    .unwrap(); // mapping on the file's current block B

  // A concurrent writer grows f, reallocating it to a new block B' and freeing B — the guest is not revoked.
  volume
    .write(&mut store, f, 0, &vec![0x22; DAX_PAGE * 4])
    .unwrap();

  // The guest's in-flight hardware write hits the stale block B, not f's live block B'.
  mapper.guest_write_raw(&mut store, &m, 0, &[GUEST; 100]);

  let mut got = vec![0u8; 100];
  volume.read(&store, f, 0, &mut got).unwrap();
  assert!(
    got.iter().all(|&b| b == GUEST),
    "LOST UPDATE: a block move under a live mapping silently discarded the guest's write to its OWN file (f reads {:?}, not the guest's bytes); the move must revoke and quiesce the guest before freeing the block",
    &got[..8]
  );
}

/// TEARDOWN / DEADLOCK-ADJACENT. Guarantee: once a mapping is revoked, the guest can no longer reach the block, so
/// the block is safe to free and reuse. Fails: `revoke` is daemon-side bookkeeping (it drops the record); it does
/// not tear down the guest's page table, so after `revoke` returns true a pile of hardware writes still corrupt the
/// block's reuser. This is also the ordering whose naive lock-based form deadlocks — the free waits for the teardown
/// to quiesce the guest while the guest faults into the daemon — so the gate must revoke+quiesce, then free, with a
/// pinned block in between.
#[test]
#[ignore = "DEMONSTRATION: fails on purpose — revoke does not quiesce the guest's hardware mapping before a free until pinning + revocation exist"]
fn writable_dax_revoke_must_quiesce_before_the_block_is_freed() {
  const POISON: u8 = 0xEE;
  const VICTIM: u8 = 0x5A;
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let root = volume.root_inode(&store).unwrap();

  let mut mapper = DaxMapper::new();
  let m = mapper
    .setup_write_mapping(&mut store, &mut volume, f)
    .unwrap();

  // The daemon revokes the mapping — it believes the guest can no longer reach the block.
  assert!(mapper.revoke(&m), "the mapping was revoked");

  // ...but the block is freed and reused while the guest's page table is still standing.
  volume.unlink_no(&mut store, root, "f").unwrap();
  let g = file_of(&mut store, &mut volume, "g", VICTIM, DAX_PAGE);
  let gblock = mapper
    .setup_read_mapping(&mut store, &volume, g)
    .unwrap()
    .block;
  assert!(
    gblock.region() == m.block.region() && gblock.offset() == m.block.offset(),
    "the victim reused the revoked mapping's block (the teardown-gap precondition)"
  );

  // A guest ignoring the revoke writes the hardware way — revoke did not stop it.
  mapper.guest_write_raw(&mut store, &m, 0, &[POISON; 100]);
  let mut got = vec![0u8; 100];
  volume.read(&store, g, 0, &mut got).unwrap();
  assert!(
    got.iter().all(|&b| b == VICTIM),
    "REVOKE IS A NO-OP ON THE HARDWARE PATH: after revoke() returned true a guest still corrupted the block's reuser (got {:?}); revoke must tear down the guest page table and the block must stay pinned until that teardown quiesces",
    &got[..8]
  );
}

/// RACE / PILE (many concurrent broken writers). Guarantee: no live file is ever touched by a guest whose own file
/// is gone. Fails: a pile of guests map writable, their files are freed, a live victim reuses one of the blocks, and
/// then every stale guest hammers its old mapping the hardware way. The victim that reused a freed block is
/// corrupted by whichever stale guest held it — every freed-but-still-mapped block is a loaded gun.
#[test]
#[ignore = "DEMONSTRATION: fails on purpose — a pile of stale hardware writers corrupts a live file until pinning + revocation exist"]
fn writable_dax_a_pile_of_stale_writers_must_not_corrupt_a_live_file() {
  const POISON: u8 = 0xEE;
  const VICTIM: u8 = 0x5A;
  const PILE: usize = 16;
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  let mut mapper = DaxMapper::new();

  let mut stale = Vec::new();
  for i in 0..PILE {
    let name = format!("p{i}");
    let f = file_of(&mut store, &mut volume, &name, 0x11, DAX_PAGE);
    let m = mapper
      .setup_write_mapping(&mut store, &mut volume, f)
      .unwrap();
    stale.push((name, m));
  }
  for (name, _) in &stale {
    volume.unlink_no(&mut store, root, name).unwrap();
  }

  // One live victim reuses one of the freed blocks.
  let v = file_of(&mut store, &mut volume, "victim", VICTIM, DAX_PAGE);
  let vblock = mapper
    .setup_read_mapping(&mut store, &volume, v)
    .unwrap()
    .block;
  assert!(
    stale
      .iter()
      .any(|(_, m)| m.block.region() == vblock.region() && m.block.offset() == vblock.offset()),
    "the victim reused one of the piled-up freed blocks (the pile precondition)"
  );

  // The pile: every stale guest writes poison the hardware way.
  for (_, m) in &stale {
    mapper.guest_write_raw(&mut store, m, 0, &[POISON; DAX_PAGE]);
  }

  let mut got = vec![0u8; DAX_PAGE];
  volume.read(&store, v, 0, &mut got).unwrap();
  assert!(
    got.iter().all(|&b| b == VICTIM),
    "PILE ESCAPE: one of {PILE} piled stale hardware writes corrupted a live file that reused a freed block (victim holds {:?}); every freed-but-still-mapped block is live until pinning + revocation",
    &got[..8]
  );
}
