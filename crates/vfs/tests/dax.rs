//! The DAX mapping gate proven by use (§4.6, A-9, AC-4.12: mapping isolation, pinning and teardown). Every test is
//! an attack a hostile guest would try through the shared window over the *real* content arena — exfiltration,
//! injection, volume escape, a concurrent pile, TOCTOU, a block move, a snapshot share, teardown — and each now
//! holds, because the gate works at the memory level, not by a per-access check a hardware DAX guest bypasses:
//!
//!   - **Pinning** (`slates_mem::arena::ChunkArena::pin`, exercised here through `Volume::dax_map_*`): a mapped block
//!     is never reused or freed while mapped, so the block a mapping names can never become another file's — in this
//!     volume or any other on the shard's shared arena. This is what closes exfiltration, injection, the volume
//!     escape, the pile and the TOCTOU reuse.
//!   - **Teardown** (`Store::dax_revoke*`, driven from `write`/`truncate`/`snapshot`): before content a mapping covers
//!     is moved, shared or reclaimed, the mapping is revoked and its window torn down, so a stale hardware access is
//!     inert (modeled by `Store::dax_read_raw`/`dax_write_raw` going quiet for a revoked mapping — the MMU, not the
//!     daemon, enforces this). This closes the block-move lost update and the snapshot-share corruption.
//!
//! The commit history on this branch has the pre-gate form of each test, which *failed* — the escapes were real
//! before pinning and teardown existed.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use slates_vfs::dax::DAX_PAGE;
use slates_vfs::ids::InodeNo;
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{clone_config, store, volume};

/// A byte that stands for a secret a neighbour holds — if a guest reads it through another file's mapping, that is a
/// leak.
const SECRET: u8 = 0xAB;
/// A byte a hostile guest writes, to see whether it reaches anything it should not.
const POISON: u8 = 0xEE;

/// Creates a regular file of `fill`×`len` bytes in the root (large enough to live in an arena block, not inline),
/// returns its inode number.
fn file_of(store: &mut Store, volume: &mut Volume, name: &str, fill: u8, len: usize) -> InodeNo {
  let root = volume.root_inode(store).unwrap();
  let no = volume.create_file_no(store, root, name, 0o644).unwrap();
  volume.write(store, no, 0, &vec![fill; len]).unwrap();
  no
}

// ───────────────────────────────────────────────────────────── daemon-mediated guards (the control path)

/// §4.6. Do: write a page of a secret, truncate to 100 bytes, map read, read the whole page. Expect: bytes 100..4096
/// read as zero — the mapping scrubbed the block's tail past the file, so the guest's whole-page view never reaches a
/// truncate's leftover bytes.
#[test]
fn a_mapping_does_not_expose_the_bytes_past_the_files_length() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let secret = file_of(&mut store, &mut volume, "secret", SECRET, DAX_PAGE);
  volume.truncate(&mut store, secret, 100).unwrap();

  let mapping = volume.dax_map_read(&mut store, secret).unwrap();
  assert_eq!(mapping.file_len, 100, "the authorized length is the truncated size");
  let page = volume.dax_read(&store, &mapping, 0, DAX_PAGE).unwrap();
  assert!(
    page[100..].iter().all(|&b| b == 0),
    "the bytes past the file's new length are zero, not the truncated-away secret: {:?}",
    &page[100..120]
  );
}

/// §4.6. Do: map a 100-byte file writable, write at and past the length through the daemon path. Expect: a write
/// within the length is allowed, one past it or straddling its end is refused — growing a file needs a quota and lease
/// check the guest cannot make itself.
#[test]
fn a_daemon_write_past_the_authorized_length_is_refused() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x44, DAX_PAGE);
  volume.truncate(&mut store, f, 100).unwrap();
  let m = volume.dax_map_write(&mut store, f).unwrap();
  assert_eq!(m.file_len, 100);
  assert!(volume.dax_write(&mut store, &m, 0, &[0x33; 100]).is_ok());
  assert!(
    volume.dax_write(&mut store, &m, 100, &[0x33; 16]).is_err(),
    "a write past the length is refused"
  );
  assert!(
    volume.dax_write(&mut store, &m, 95, &[0x33; 16]).is_err(),
    "a write straddling the end is refused whole"
  );
}

/// §4.6. Do: map a file read-only, then write through it. Expect: refused — a read-only mapping is never a write path.
#[test]
fn a_write_through_a_read_only_mapping_is_refused() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let m = volume.dax_map_read(&mut store, f).unwrap();
  assert!(volume.dax_write(&mut store, &m, 0, &[0x99; 10]).is_err());
}

/// §4.6. Do: map the root directory. Expect: refused — only a regular file's single content block may be mapped.
#[test]
fn mapping_a_directory_is_refused() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let root = volume.root_inode(&store).unwrap();
  assert!(volume.dax_map_read(&mut store, root).is_err());
}

/// §4.6 (CLAUDE.md #8, no unbounded growth / no panic). Do: colossal and overflowing offsets and lengths on a valid
/// mapping. Expect: no panic, no out-of-bounds — a read past the mapped span is refused before any allocation, an
/// overflowing write is refused.
#[test]
fn colossal_and_overflowing_offsets_are_refused_without_a_panic() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let m = volume.dax_map_write(&mut store, f).unwrap();
  assert!(volume.dax_read(&store, &m, usize::MAX - 10, 100).is_err());
  assert!(volume.dax_read(&store, &m, 0, usize::MAX).is_err());
  assert!(volume.dax_read(&store, &m, 0, m.page_span() + DAX_PAGE).is_err());
  assert!(volume.dax_write(&mut store, &m, usize::MAX, &[0u8; 10]).is_err());
}

// ───────────────────────────────────────────────────────────── pinning (the memory-level gate)

/// §4.6, AC-4.12 (pinning closes exfiltration). Do: map a file read-only, unlink it, then create a different file
/// that would have reused its block, and read through the still-held mapping the hardware way. Expect: the new file
/// does NOT get the mapped block (it is pinned), and the hardware read returns the mapped file's own retained bytes,
/// never the neighbour's secret. Before the gate the neighbour reused the freed block and the read leaked it.
#[test]
fn pinning_keeps_an_unlinked_mapped_file_from_leaking_into_a_neighbour() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let root = volume.root_inode(&store).unwrap();
  let m = volume.dax_map_read(&mut store, f).unwrap();

  volume.unlink_no(&mut store, root, "f").unwrap();
  let g = file_of(&mut store, &mut volume, "g", SECRET, DAX_PAGE);
  let gm = volume.dax_map_read(&mut store, g).unwrap();
  assert!(
    gm.block.region() != m.block.region() || gm.block.offset() != m.block.offset(),
    "pinning kept the neighbour off the mapped block"
  );

  let raw = store.dax_read_raw(&m, 0, DAX_PAGE);
  assert!(
    !raw.contains(&SECRET),
    "the hardware read did not reach the neighbour's secret"
  );
  assert!(
    raw[..100].iter().all(|&b| b == 0x11),
    "it reads the mapped file's own retained bytes"
  );
}

/// §4.6, AC-4.12 (pinning closes cross-file injection). Do: map a file writable, unlink it, create a neighbour that
/// would reuse its block, then write through the stale mapping the hardware way. Expect: the neighbour is untouched —
/// it never got the pinned block, so the write lands only on the mapped file's own (orphaned) block.
#[test]
fn a_guest_cannot_corrupt_a_neighbour_because_the_block_is_pinned() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  const NEIGHBOUR: u8 = 0x5A;
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let root = volume.root_inode(&store).unwrap();
  let m = volume.dax_map_write(&mut store, f).unwrap();

  volume.unlink_no(&mut store, root, "f").unwrap();
  let g = file_of(&mut store, &mut volume, "g", NEIGHBOUR, DAX_PAGE);

  store.dax_write_raw(&m, 0, &[POISON; 100]);
  let mut got = vec![0u8; 100];
  volume.read(&store, g, 0, &mut got).unwrap();
  assert!(
    got.iter().all(|&b| b == NEIGHBOUR),
    "the neighbour is untouched (got {:?})",
    &got[..8]
  );
}

/// §4.6, AC-4.12 (pinning confines a guest to its volume). Do: two volumes on one shard arena. The attacker maps a
/// file writable and unlinks it; the victim volume allocates a file that would reuse the block; the attacker writes
/// the stale mapping the hardware way. Expect: the victim volume's file is untouched — the shared arena never handed
/// the pinned block across the boundary. Before the gate this was a cross-tenant corruption.
#[test]
fn a_guest_cannot_escape_into_another_volume() {
  let mut store = store();
  let mut attacker = volume(&mut store, 1 << 20);
  let mut victim = Volume::create(&mut store, clone_config(9)).unwrap();
  const VICTIM: u8 = 0x5A;

  let fa = file_of(&mut store, &mut attacker, "fa", 0x11, DAX_PAGE);
  let root_a = attacker.root_inode(&store).unwrap();
  let m = attacker.dax_map_write(&mut store, fa).unwrap();
  attacker.unlink_no(&mut store, root_a, "fa").unwrap();

  let fb = file_of(&mut store, &mut victim, "fb", VICTIM, DAX_PAGE);
  let vm = victim.dax_map_read(&mut store, fb).unwrap();
  assert!(
    vm.block.region() != m.block.region() || vm.block.offset() != m.block.offset(),
    "the victim volume did not get the attacker's pinned block"
  );

  store.dax_write_raw(&m, 0, &[POISON; 100]);
  let mut got = vec![0u8; 100];
  victim.read(&store, fb, 0, &mut got).unwrap();
  assert!(
    got.iter().all(|&b| b == VICTIM),
    "the other volume's file is untouched (got {:?})",
    &got[..8]
  );
}

/// §4.6, AC-4.12 (pinning confines reads to the volume). Do: as the escape above, but the attacker *reads* the stale
/// mapping the hardware way after the victim volume allocates a secret. Expect: the attacker reads its own file's
/// bytes, never the other tenant's secret.
#[test]
fn a_guest_cannot_read_across_a_volume_boundary() {
  let mut store = store();
  let mut attacker = volume(&mut store, 1 << 20);
  let mut victim = Volume::create(&mut store, clone_config(9)).unwrap();

  let fa = file_of(&mut store, &mut attacker, "fa", 0x11, DAX_PAGE);
  let root_a = attacker.root_inode(&store).unwrap();
  let m = attacker.dax_map_read(&mut store, fa).unwrap();
  attacker.unlink_no(&mut store, root_a, "fa").unwrap();

  let fb = file_of(&mut store, &mut victim, "fb", SECRET, DAX_PAGE);
  let _vm = victim.dax_map_read(&mut store, fb).unwrap();

  let raw = store.dax_read_raw(&m, 0, DAX_PAGE);
  assert!(
    !raw.contains(&SECRET),
    "the hardware read did not cross the volume boundary"
  );
}

/// §4.6, AC-4.12 (pinning under a pile of concurrent broken writers). Do: many guests map files writable and unlink
/// them; a live victim allocates a file; then every stale mapping hammers its block the hardware way. Expect: the
/// victim never got any pinned block, and not one stale write reaches it. Before the gate one of the pile reused the
/// victim's block and corrupted it.
#[test]
fn a_pile_of_stale_writers_cannot_corrupt_a_live_file() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  const PILE: usize = 16;
  const VICTIM: u8 = 0x5A;

  let mut stale = Vec::new();
  for i in 0..PILE {
    let name = format!("p{i}");
    let f = file_of(&mut store, &mut volume, &name, 0x11, DAX_PAGE);
    let m = volume.dax_map_write(&mut store, f).unwrap();
    stale.push((name, m));
  }
  for (name, _) in &stale {
    volume.unlink_no(&mut store, root, name).unwrap();
  }

  let v = file_of(&mut store, &mut volume, "victim", VICTIM, DAX_PAGE);
  let vm = volume.dax_map_read(&mut store, v).unwrap();
  assert!(
    stale.iter().all(|(_, m)| m.block.region() != vm.block.region()
      || m.block.offset() != vm.block.offset()),
    "the victim did not reuse any pinned block"
  );
  for (_, m) in &stale {
    store.dax_write_raw(m, 0, &[POISON; DAX_PAGE]);
  }
  let mut got = vec![0u8; DAX_PAGE];
  volume.read(&store, v, 0, &mut got).unwrap();
  assert!(
    got.iter().all(|&b| b == VICTIM),
    "no stale writer reached the live file (got {:?})",
    &got[..8]
  );
}

/// §4.6, AC-4.12 (pinning closes the TOCTOU window). Do: map a file writable, unlink it, create a neighbour; the
/// daemon-mediated access is refused (the file is gone — the check sees it), and the hardware access runs unchecked.
/// Expect: the neighbour is untouched anyway — pinning means the block the stale mapping names is never the
/// neighbour's, so the check and the use cannot disagree in a way that harms. (The check-time refusal is asserted too,
/// so the test is not vacuous.)
#[test]
fn the_check_and_the_hardware_use_cannot_disagree_because_the_block_is_pinned() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  const NEIGHBOUR: u8 = 0x5A;
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let root = volume.root_inode(&store).unwrap();
  let m = volume.dax_map_write(&mut store, f).unwrap();

  volume.unlink_no(&mut store, root, "f").unwrap();
  let g = file_of(&mut store, &mut volume, "g", NEIGHBOUR, DAX_PAGE);

  assert!(
    volume.dax_write(&mut store, &m, 0, &[POISON; 50]).is_err(),
    "the daemon-mediated check refuses the stale mapping"
  );
  store.dax_write_raw(&m, 0, &[POISON; 50]);
  let mut got = vec![0u8; 50];
  volume.read(&store, g, 0, &mut got).unwrap();
  assert!(
    got.iter().all(|&b| b == NEIGHBOUR),
    "the hardware use reached only the pinned orphan block, never the neighbour (got {:?})",
    &got[..8]
  );
}

// ───────────────────────────────────────────────────────────── teardown (revocation)

/// §4.6, AC-4.12 (teardown closes the revoke-then-reuse hole). Do: map a file writable, revoke it, unlink the file so
/// the now-unpinned block is freed, let a neighbour reuse it, then write the revoked mapping the hardware way. Expect:
/// the neighbour is untouched — a revoked mapping's window is torn down, so its hardware access is inert. Before the
/// gate `revoke` was daemon bookkeeping and the write still corrupted the reuser.
#[test]
fn a_revoked_mapping_is_inert_on_the_hardware_path() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  const NEIGHBOUR: u8 = 0x5A;
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let root = volume.root_inode(&store).unwrap();
  let m = volume.dax_map_write(&mut store, f).unwrap();

  assert!(store.dax_revoke(&m), "the mapping was revoked");
  assert!(!store.dax_is_live(&m));
  volume.unlink_no(&mut store, root, "f").unwrap();
  let g = file_of(&mut store, &mut volume, "g", NEIGHBOUR, DAX_PAGE);
  let gm = volume.dax_map_read(&mut store, g).unwrap();
  assert!(
    gm.block.region() == m.block.region() && gm.block.offset() == m.block.offset(),
    "with the mapping revoked the block is freed and reused (no leak)"
  );

  store.dax_write_raw(&m, 0, &[POISON; 100]);
  let mut got = vec![0u8; 100];
  volume.read(&store, g, 0, &mut got).unwrap();
  assert!(
    got.iter().all(|&b| b == NEIGHBOUR),
    "the revoked mapping's hardware write was inert (got {:?})",
    &got[..8]
  );
}

/// §4.6, AC-4.12 (a block move revokes the mapping — the race/lost-update is closed). Do: map a file writable, then
/// grow it through the VFS so its block reallocates. Expect: the grow revoked the mapping, so the stale hardware write
/// is inert and the file keeps its VFS-written content — no silent corruption, no write misdirected to a stranger.
#[test]
fn a_block_move_revokes_the_mapping_and_the_stale_hardware_write_is_inert() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let m = volume.dax_map_write(&mut store, f).unwrap();

  let before = store.dax.revoked();
  volume.write(&mut store, f, 0, &vec![0x22; DAX_PAGE * 4]).unwrap();
  assert!(
    !store.dax_is_live(&m) && store.dax.revoked() > before,
    "the VFS write revoked the mapping"
  );

  store.dax_write_raw(&m, 0, &[POISON; 100]);
  let mut got = vec![0u8; 100];
  volume.read(&store, f, 0, &mut got).unwrap();
  assert!(
    got.iter().all(|&b| b == 0x22),
    "the file keeps its VFS content; the stale write was inert (got {:?})",
    &got[..8]
  );
}

/// §4.6, AC-4.12 (a block move leaves the daemon path a typed miss). Do: map writable, grow through the VFS, then use
/// the old mapping on the daemon path. Expect: refused — the mapping is revoked.
#[test]
fn the_daemon_path_refuses_a_mapping_whose_block_moved() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  volume.truncate(&mut store, f, 100).unwrap();
  let m = volume.dax_map_write(&mut store, f).unwrap();
  volume.write(&mut store, f, 0, &vec![0x22; DAX_PAGE * 4]).unwrap();
  assert!(volume.dax_write(&mut store, &m, 0, &[0x33; 50]).is_err());
  assert!(volume.dax_read(&store, &m, 0, 50).is_err());
}

/// §4.6, AC-4.12 (a snapshot taken after a writable map revokes it before sharing). Do: map a file writable, snapshot
/// the volume, then write the old mapping the hardware way. Expect: the snapshot shares a block the mapping no longer
/// holds — the snapshot takes its value, and the live file too, with the stale write inert. Before the gate the
/// hardware write corrupted the snapshot despite a daemon-side re-copy.
#[test]
fn a_snapshot_after_a_writable_map_revokes_it_and_is_not_corrupted() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  const ORIG: u8 = 0x11;
  let f = file_of(&mut store, &mut volume, "f", ORIG, DAX_PAGE);
  let m = volume.dax_map_write(&mut store, f).unwrap();

  let snap = volume.snapshot(&mut store).unwrap();
  assert!(!store.dax_is_live(&m), "the snapshot revoked the writable mapping");

  store.dax_write_raw(&m, 0, &[POISON; 100]);
  let mut frozen = vec![0u8; 100];
  volume.read_in(&store, snap, f, 0, &mut frozen).unwrap();
  assert!(
    frozen.iter().all(|&b| b == ORIG),
    "the snapshot keeps its bytes (got {:?})",
    &frozen[..8]
  );
  let mut live = vec![0u8; 100];
  volume.read(&store, f, 0, &mut live).unwrap();
  assert!(live.iter().all(|&b| b == ORIG), "the live file is uncorrupted too");
}

/// §4.6 (a writable map taken after a snapshot copies up — the frozen version is safe without a revoke). Do: snapshot
/// a file, then map it writable (forcing a copy-up to a private block) and write it through the daemon path. Expect:
/// the snapshot keeps its original bytes — the guest writes a block only the live version names.
#[test]
fn a_writable_map_after_a_snapshot_copies_up_and_spares_the_snapshot() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  const ORIG: u8 = 0x11;
  const GUEST: u8 = 0x22;
  let f = file_of(&mut store, &mut volume, "f", ORIG, DAX_PAGE);
  let snap = volume.snapshot(&mut store).unwrap();

  let m = volume.dax_map_write(&mut store, f).unwrap();
  volume.dax_write(&mut store, &m, 0, &[GUEST; 200]).unwrap();

  let mut frozen = vec![0u8; DAX_PAGE];
  volume.read_in(&store, snap, f, 0, &mut frozen).unwrap();
  assert!(
    frozen.iter().all(|&b| b == ORIG),
    "the snapshot keeps its original bytes (got {:?})",
    &frozen[..8]
  );
}

/// §4.6 (the allowed DAX case: immutable shared content). Do: map a file read-only, then snapshot the volume. Expect:
/// the read-only mapping survives — a snapshot shares its block as immutable content, exactly what DAX may map while
/// a version is pinned — and the guest keeps reading its own bytes. Only writable mappings are revoked by a snapshot.
#[test]
fn a_read_only_mapping_survives_a_snapshot() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  const ORIG: u8 = 0x11;
  let f = file_of(&mut store, &mut volume, "f", ORIG, DAX_PAGE);
  let m = volume.dax_map_read(&mut store, f).unwrap();
  let _snap = volume.snapshot(&mut store).unwrap();
  assert!(store.dax_is_live(&m), "a read-only mapping is not revoked by a snapshot");
  let raw = store.dax_read_raw(&m, 0, 100);
  assert!(raw.iter().all(|&b| b == ORIG), "it still reads its own bytes");
}

// ───────────────────────────────────────────────────────────── lifecycle (reclamation)

/// §4.6, AC-4.12 ("eventual reclamation"). Do: map a file read-only, unlink it (its block is pinned and kept), then
/// revoke the mapping. Expect: the deferred free runs on the last unpin, so a new file reuses the block — a mapping
/// never leaks its block past its own life.
#[test]
fn unmapping_a_deleted_file_releases_its_block_for_reuse() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let root = volume.root_inode(&store).unwrap();
  let m = volume.dax_map_read(&mut store, f).unwrap();
  volume.unlink_no(&mut store, root, "f").unwrap();

  assert!(store.dax_revoke(&m), "revoked");
  let g = file_of(&mut store, &mut volume, "g", 0x5A, DAX_PAGE);
  let gm = volume.dax_map_read(&mut store, g).unwrap();
  assert!(
    gm.block.region() == m.block.region() && gm.block.offset() == m.block.offset(),
    "the unpinned block was freed and reused"
  );
}

/// §4.6, AC-4.12 (A-99: a past-length write into the page tail is scrubbed before the block is reused). Do: map a
/// 100-byte file writable, write poison into the page tail past the length the hardware way, unlink the file and
/// revoke the mapping (the last unpin scrubs the block), then let a new file reuse it. Expect: the new file reads
/// zero, never the poisoned tail — the tail never outlives the mapping.
#[test]
fn a_past_length_tail_write_is_scrubbed_before_the_block_is_reused() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  volume.truncate(&mut store, f, 100).unwrap();
  let root = volume.root_inode(&store).unwrap();
  let m = volume.dax_map_write(&mut store, f).unwrap();

  store.dax_write_raw(&m, 100, &[POISON; 200]); // into the page tail, past the file
  volume.unlink_no(&mut store, root, "f").unwrap();
  assert!(store.dax_revoke(&m), "revoke runs the deferred free, scrubbing the block");

  let g = file_of(&mut store, &mut volume, "g", 0x00, DAX_PAGE);
  let gm = volume.dax_map_read(&mut store, g).unwrap();
  assert!(
    gm.block.region() == m.block.region() && gm.block.offset() == m.block.offset(),
    "the new file reused the released block"
  );
  let raw = store.dax_read_raw(&gm, 0, DAX_PAGE);
  assert!(
    !raw.contains(&POISON),
    "the poisoned tail was scrubbed before the block was reused"
  );
}

/// §4.6 (non-vacuity of teardown). Do: map several files, then revoke them all. Expect: the registry empties and the
/// revoke counter moves by exactly the number mapped — a dead teardown path cannot pass for a live one.
#[test]
fn revocation_empties_the_registry_and_counts() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 24);
  let mut ms = Vec::new();
  for i in 0..8 {
    let f = file_of(&mut store, &mut volume, &format!("f{i}"), 0x11, DAX_PAGE);
    ms.push(volume.dax_map_read(&mut store, f).unwrap());
  }
  assert_eq!(store.dax_mappings(), 8);
  let before = store.dax.revoked();
  for m in &ms {
    assert!(store.dax_revoke(m));
  }
  assert_eq!(store.dax_mappings(), 0, "the registry is empty");
  assert_eq!(store.dax.revoked() - before, 8, "every revoke was counted");
}
