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

use proptest::prelude::*;
use slates_vfs::dax::{DAX_PAGE, DaxScrub, Mapping, stripe_target};
use slates_vfs::error::VfsError;
use slates_vfs::ids::InodeNo;
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{clone_config, store, store_capped_chunks, store_sandboxed};

/// The gate's standard test volume, **with writable DAX opted in** (L0). The mapping-gate tests below exercise what a
/// writable mapping does *after* a volume opts in — pinning, teardown, migration — so the shared fixture opts in. The
/// opt-in policy itself (writable DAX refused by default, read-only served) is proven separately by
/// `a_writable_dax_map_is_refused_until_the_volume_opts_in`, which uses a plain `common::volume`.
fn volume(store: &mut Store, quota: u64) -> Volume {
  let mut v = common::volume(store, quota);
  v.enable_writable_dax();
  v
}

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
  assert_eq!(
    mapping.file_len, 100,
    "the authorized length is the truncated size"
  );
  let page = volume.dax_read(&store, &mapping, 0, DAX_PAGE).unwrap();
  assert!(
    page[100..].iter().all(|&b| b == 0),
    "the bytes past the file's new length are zero, not the truncated-away secret: {:?}",
    &page[100..120]
  );
}

/// §4.6 (A-135, inline DAX). Do: write a small file whose bytes stay inline in the inode (no content block), map it
/// read-only, read the mapped window. Expect: the map succeeds (before this it was refused `Invalid` and the guest
/// fell back to FUSE_READ), the window shows the inline bytes with the page tail scrubbed to zero — an inline file
/// is DAX-mappable through a throwaway block the mapping owns, exactly as a directory's entries are.
#[test]
fn an_inline_file_is_mapped_read_only_through_a_materialized_block() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let root = volume.root_inode(&store).unwrap();
  let no = volume
    .create_file_no(&mut store, root, "tiny", 0o644)
    .unwrap();
  // 48 bytes: well under `inline_bytes` (~128 B), so the content stays in the inode with no arena content block.
  let bytes = [0x5Au8; 48];
  volume.write(&mut store, no, 0, &bytes).unwrap();

  let mapping = volume.dax_map_read(&mut store, no).unwrap();
  assert_eq!(
    mapping.file_len,
    bytes.len(),
    "the authorized length is the inline file's size"
  );
  let page = volume.dax_read(&store, &mapping, 0, DAX_PAGE).unwrap();
  assert_eq!(
    &page[..bytes.len()],
    &bytes,
    "the window shows the inline file's bytes"
  );
  assert!(
    page[bytes.len()..].iter().all(|&b| b == 0),
    "the page tail past the inline file is scrubbed to zero: {:?}",
    &page[bytes.len()..bytes.len() + 16]
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

/// §4.6 multi-tenancy **L0** (the Rowhammer gate's outermost layer; the "#3 active address: writable-DAX opt-in /
/// read-only default" the hardening owed). Do: on a plain volume that has *not* opted in, map a file read-only, then
/// writable; opt in; map writable again. Expect: read-only always succeeds (immutable shared content a guest reaches
/// but cannot hammer into a neighbour is the safe default), a writable map is refused `NotPermitted` until the opt-in,
/// and is served after — so the one DAX mode a guest can Rowhammer stays off unless the owner deliberately enables it.
#[test]
fn a_writable_dax_map_is_refused_until_the_volume_opts_in() {
  use slates_vfs::error::VfsError;
  let mut store = store();
  let mut plain = common::volume(&mut store, 1 << 20); // the plain fixture, not the opted-in `volume` wrapper above
  let f = file_of(&mut store, &mut plain, "f", 0x11, DAX_PAGE);

  assert!(
    plain.dax_map_read(&mut store, f).is_ok(),
    "read-only DAX is the default and needs no opt-in"
  );
  assert!(
    matches!(
      plain.dax_map_write(&mut store, f),
      Err(VfsError::NotPermitted)
    ),
    "a writable map is refused until the volume opts in"
  );
  assert!(!plain.writable_dax_enabled(), "still not opted in");

  plain.enable_writable_dax();
  assert!(
    plain.dax_map_write(&mut store, f).is_ok(),
    "after opt-in a writable map is served"
  );
}

/// §4.6. Do: map the root directory. Expect: refused — only a regular file's single content block may be mapped.
#[test]
fn mapping_a_directory_is_refused() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let root = volume.root_inode(&store).unwrap();
  assert!(volume.dax_map_read(&mut store, root).is_err());
}

/// Defence in depth for the device seam: a DAX map of an attribute (xattr-value) inode is refused `NotFound`,
/// never a window onto an attribute's bytes. A file's xattr lives in its own attribute inode and the map verbs
/// resolve the raw inode number, so without the `namespace_inode` gate a crafted or confused map of the attribute's
/// number would reach its content — the same `namespace_inode` refusal the NFS/FUSE transports apply, now mirrored
/// in the gate. Do: give a file an xattr, take the attribute inode's number, map it for read and for write. Expect:
/// both `NotFound`; the file's own content still maps (non-vacuity).
#[test]
fn a_dax_map_of_an_attribute_inode_is_refused() {
  use slates_vfs::error::VfsError;
  use slates_vfs::xattr::XattrSet;
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let file = file_of(&mut store, &mut volume, "f", SECRET, DAX_PAGE);
  volume
    .xattr_set(
      &mut store,
      file,
      b"user.secret",
      b"attribute value bytes",
      XattrSet::Either,
    )
    .unwrap();
  let attr = volume.xattr_inode(&store, file, b"user.secret").unwrap();
  assert!(
    matches!(
      volume.dax_map_read(&mut store, attr),
      Err(VfsError::NotFound)
    ),
    "an attribute inode cannot be DAX-mapped for read"
  );
  assert!(
    matches!(
      volume.dax_map_write(&mut store, attr),
      Err(VfsError::NotFound)
    ),
    "an attribute inode cannot be DAX-mapped for write"
  );
  assert!(
    volume.dax_map_read(&mut store, file).is_ok(),
    "non-vacuity: the file's own content still maps"
  );
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
  assert!(
    volume
      .dax_read(&store, &m, 0, m.page_span() + DAX_PAGE)
      .is_err()
  );
  assert!(
    volume
      .dax_write(&mut store, &m, usize::MAX, &[0u8; 10])
      .is_err()
  );
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

/// §4.6, AC-4.12, the tamper-proof-registry requirement (Ada: "no agent could interfere with, fabricate, or tamper
/// with said registry"). The registry — not the passed `Mapping` struct — is the source of truth: the hardware path
/// gates every access on `is_live`, an exact-identity membership test. So a mapping the daemon never minted
/// (fabricated, even onto a victim's live block) and a legitimately-minted one whose fields a caller mutated (here
/// the read-only attacker flipping `writable` to escalate) are both a different, unregistered identity, hence inert:
/// the fabricated write never reaches the victim's block and its read is zeros, and the escalated write does not
/// take effect. In the real device, the guest's page table is daemon-configured and the MMU prevents a fabricated
/// entry; `is_live` is the daemon's shadow of that, so the model and the hardware agree. (The remaining theoretical
/// case — forging the *exact* registered tuple — is now closed by construction: a `Mapping` carries a daemon-minted
/// `MappingId` whose inner is module-private, so a mapping the daemon never minted cannot be formed at all (§4.6 step 2).)
#[test]
fn a_fabricated_or_tampered_mapping_is_inert() {
  let mut store = store();
  let mut attacker = volume(&mut store, 1 << 20);
  let mut victim = Volume::create(&mut store, clone_config(9)).unwrap();
  const VICTIM: u8 = 0x5A;

  let fb = file_of(&mut store, &mut victim, "fb", VICTIM, DAX_PAGE);
  let vm = victim.dax_map_read(&mut store, fb).unwrap();
  let fa = file_of(&mut store, &mut attacker, "fa", 0x11, DAX_PAGE);
  let am = attacker.dax_map_read(&mut store, fa).unwrap();

  // Fabricated: a writable mapping onto the victim's live block, never registered. The attacker cannot forge a
  // `MappingId` (its inner is module-private), so the strongest it can do is tamper a copy of a mapping it *was*
  // minted — here its own `am`, repointed at the victim's block — which is a different, unregistered identity.
  let fabricated = Mapping {
    block: vm.block,
    writable: true,
    ..am
  };
  assert!(
    !store.dax_is_live(&fabricated),
    "a fabricated mapping is not live"
  );
  store.dax_write_raw(&fabricated, 0, &[POISON; 64]);
  let mut got = vec![0u8; 64];
  victim.read(&store, fb, 0, &mut got).unwrap();
  assert!(
    got.iter().all(|&b| b == VICTIM),
    "the fabricated write never reached the victim's block (got {:?})",
    &got[..8]
  );
  assert!(
    store
      .dax_read_raw(&fabricated, 0, DAX_PAGE)
      .iter()
      .all(|&b| b == 0),
    "the fabricated read is zeros, never the victim's bytes"
  );

  // Tampered: the attacker flips its own read-only mapping to writable — a different, unregistered identity.
  let escalated = Mapping {
    writable: true,
    ..am
  };
  assert!(
    !store.dax_is_live(&escalated),
    "flipping writable yields an unregistered mapping"
  );
  store.dax_write_raw(&escalated, 0, &[POISON; 64]);
  let mut own = vec![0u8; 64];
  attacker.read(&store, fa, 0, &mut own).unwrap();
  assert!(
    own.iter().all(|&b| b == 0x11),
    "the escalated write did not take effect on the read-only-mapped file"
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
    stale
      .iter()
      .all(|(_, m)| m.block.region() != vm.block.region() || m.block.offset() != vm.block.offset()),
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
  volume
    .write(&mut store, f, 0, &vec![0x22; DAX_PAGE * 4])
    .unwrap();
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
  volume
    .write(&mut store, f, 0, &vec![0x22; DAX_PAGE * 4])
    .unwrap();
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
  assert!(
    !store.dax_is_live(&m),
    "the snapshot revoked the writable mapping"
  );

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
  assert!(
    live.iter().all(|&b| b == ORIG),
    "the live file is uncorrupted too"
  );
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
  assert!(
    store.dax_is_live(&m),
    "a read-only mapping is not revoked by a snapshot"
  );
  let raw = store.dax_read_raw(&m, 0, 100);
  assert!(
    raw.iter().all(|&b| b == ORIG),
    "it still reads its own bytes"
  );
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
  assert!(
    store.dax_revoke(&m),
    "revoke runs the deferred free, scrubbing the block"
  );

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

/// §4.6 A-9 / AC-4.12 — the asynchronous-revocation ordering (multi-tenancy threat #1, the one cross-tenant hazard
/// left once spatial isolation holds). A mapping a live DAX guest holds must keep its block PINNED through phase-one
/// revoke (the registry entry gone, the hardware path inert, no new access authorized) and become reusable only at
/// phase-two release — which the device calls after the VMM has unmapped the guest's window and flushed every
/// physical core's TLB. If the block were freed at phase one, a neighbour taking it could be read or written through
/// the guest's stale TLB entry. Do: map a file, revoke it pending, then release. Expect: inert-but-pinned after
/// pending; reusable only after release; a double release refused.
#[test]
fn a_pending_revoke_keeps_the_block_pinned_until_the_unmap_is_confirmed() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let no = file_of(&mut store, &mut volume, "f", SECRET, DAX_PAGE);
  let mapping = volume.dax_map_read(&mut store, no).unwrap();
  let block = mapping.block;
  assert!(
    store.content.arena().is_pinned(block),
    "a live mapping pins its block"
  );

  assert!(
    store.dax_revoke_pending(&mapping),
    "phase one: the mapping was held"
  );
  assert!(
    !store.dax_is_live(&mapping),
    "phase one: the hardware path is inert — no new access authorized"
  );
  assert!(
    store.content.arena().is_pinned(block),
    "phase one KEEPS the pin: the block cannot be reused until the VMM confirms the unmap + cross-core TLB flush"
  );

  assert!(
    store.dax_release_after_unmap(&mapping),
    "phase two releases the pin, only after the confirmed unmap"
  );
  assert!(
    !store.content.arena().is_pinned(block),
    "only now, after the confirmed unmap, is the block reusable"
  );
  let refusals_before = store.release_refusals;
  assert!(
    !store.dax_release_after_unmap(&mapping),
    "a double release is refused, never a double-free"
  );
  // And it is *counted*. `release_refusals` is the signal the adversarial suites now assert is zero (the device
  // chaos schedule, the hostile request sequence, both pin-ledger tests); a counter that could never move would
  // make every one of those pass for nothing. This is the one place that proves it moves.
  assert_eq!(
    store.release_refusals.saturating_sub(refusals_before),
    1,
    "the refused double release is counted, so a zero asserted elsewhere means something"
  );
}

/// §4.6 multi-tenancy — adversarial: one volume's map storm must not deny another volume. Do: with a small chunk
/// budget, let volume A ask for one window over and over, then ask volume B — which has mapped nothing — for a
/// single read window. Expect: B is served. The registry's cap bounds the *shard*, and a bound one tenant can
/// consume whole is a bound every other tenant pays for.
///
/// Non-vacuity: B's map is shown to succeed on a quiet registry first, so the refusal is A's storm and not a
/// volume that could never map.
///
/// **Fixed 2026-10-18, and the fix took two attempts.** Returning the live window for a repeat request
/// ([`DaxRegistry::live_window`]) makes the registry grow with the windows that exist rather than the requests
/// made. The first attempt did only that and was withdrawn the same day: the arena's pin is refcounted **per
/// mapping**, while the device keeps one window entry per SETUPMAPPING and releases each, so two guest windows
/// sharing one mapping shared one pin and the first unmap released a block the second still mapped. The version
/// that stands pins again on every hit — one pin per window handed out — and
/// `two_windows_over_one_block_each_hold_its_pin` in `crates/bridge-virtiofs/tests/device.rs` is the guard that
/// told the two apart. Record: `docs/bugs/2026-10-18-one-volumes-dax-map-storm-denies-every-other-volume.md`.
#[test]
fn one_volumes_map_storm_does_not_deny_another_volume() {
  const CAP: usize = 32;
  let mut store = store_capped_chunks(CAP);
  let mut attacker = volume(&mut store, 1 << 20);
  let mut victim = volume(&mut store, 1 << 20);
  let loud = file_of(&mut store, &mut attacker, "loud", 0x11, DAX_PAGE);
  let quiet = file_of(&mut store, &mut victim, "quiet", 0x22, DAX_PAGE);

  let first = victim
    .dax_map_read(&mut store, quiet)
    .expect("the victim maps on a quiet registry");
  assert!(store.dax_revoke(&first));

  for _ in 0..(CAP * 4) {
    if attacker.dax_map_read(&mut store, loud).is_err() {
      break;
    }
  }
  let served = victim.dax_map_read(&mut store, quiet);
  assert!(
    served.is_ok(),
    "the victim volume was denied its window by another volume's storm: {served:?} \
     (registry holds {})",
    store.dax_mappings()
  );
}

/// §4.6 (CLAUDE.md #8, no unbounded growth) — map-storm DoS (multi-tenancy threat #2). A malicious guest cannot
/// exhaust the daemon by flooding SETUPMAPPING. Do: with a small chunk budget, re-map one file far past the
/// registry cap, then map a distinct file. Expect: the storm is **absorbed** — a request for a window that
/// already exists returns that window, so the registry grows with the windows that exist and not with the
/// requests made — while a distinct window does add an entry, and the registry never exceeds its cap.
///
/// This replaced a version asserting the storm was *refused* at the cap. Refusing met the letter of the threat
/// (nothing grew without bound) and missed its substance: the cap bounds the **shard**, so the volume that
/// filled it denied every other volume on it — measured at 32 of 32 by
/// `one_volumes_map_storm_does_not_deny_another_volume` above, which is the test that now holds that property.
/// Absorbing is the stronger guarantee: a volume consumes entries only for windows it actually has.
#[test]
fn a_map_storm_is_absorbed_and_the_registry_grows_only_with_real_windows() {
  const CAP: usize = 32;
  let mut store = store_capped_chunks(CAP);
  let mut volume = volume(&mut store, 1 << 20);
  let no = file_of(&mut store, &mut volume, "f", SECRET, DAX_PAGE);

  let first = volume
    .dax_map_read(&mut store, no)
    .expect("the first window");
  let after_first = store.dax_mappings();
  let mut same = true;
  for _ in 0..(CAP * 4) {
    match volume.dax_map_read(&mut store, no) {
      Ok(m) => same &= m == first,
      Err(e) => panic!("a repeat request for a live window must be served, not refused: {e:?}"),
    }
  }
  assert!(
    same,
    "every repeat returned the window that already existed"
  );
  assert_eq!(
    store.dax_mappings(),
    after_first,
    "{} requests past the cap added no entries at all",
    CAP * 4
  );

  // Non-vacuity: a distinct window DOES add an entry, so the dedup is not swallowing everything.
  let other = file_of(&mut store, &mut volume, "g", POISON, DAX_PAGE);
  volume
    .dax_map_read(&mut store, other)
    .expect("a distinct file's window");
  assert_eq!(
    store.dax_mappings(),
    after_first.saturating_add(1),
    "a window of a different file is a new entry"
  );
  assert!(
    store.dax_mappings() <= CAP,
    "the registry never exceeds its cap ({} > {CAP})",
    store.dax_mappings()
  );
}

/// §4.6, AC-4.12 ("eventual reclamation"). Do: map two files of a volume (one read, one write), then destroy the
/// volume. Expect: every mapping is revoked, so no mapped block is left held pinned after the volume is gone.
#[test]
fn destroying_a_volume_revokes_all_its_mappings() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let g = file_of(&mut store, &mut volume, "g", 0x22, DAX_PAGE);
  let _mf = volume.dax_map_read(&mut store, f).unwrap();
  let _mg = volume.dax_map_write(&mut store, g).unwrap();
  assert_eq!(store.dax_mappings(), 2);
  volume.destroy(&mut store).unwrap();
  assert_eq!(
    store.dax_mappings(),
    0,
    "destroy revoked every mapping of the volume"
  );
}

/// §4.6, AC-4.12 (teardown on truncate): a writable mapping held across a daemon-path truncate that SHRINKS the file
/// is revoked before the truncate frees the tail, so the guest's stale window can never reach the bytes the truncate
/// released — which the arena may scrub and reuse for another file. Map a full page writable, truncate to 100 bytes
/// through the daemon, then drive the hardware path via the STALE mapping: it is not live, its read is zeros (never
/// the freed tail), and its write is a no-op, and the file stays the truncated size.
#[test]
fn a_truncate_while_mapped_revokes_the_window_so_the_freed_tail_is_inert() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x44, DAX_PAGE);
  let m = volume.dax_map_write(&mut store, f).unwrap();
  assert_eq!(m.file_len, DAX_PAGE, "the file is mapped at its full page");

  // Truncate small through the daemon: this revokes the held mapping and frees the tail.
  volume.truncate(&mut store, f, 100).unwrap();
  assert!(
    !store.dax_is_live(&m),
    "the truncate revoked the held mapping"
  );

  // The stale window cannot reach the freed tail: its hardware read is zeros, its write a no-op.
  let tail = store.dax_read_raw(&m, 200, 100);
  assert!(
    tail.iter().all(|&b| b == 0),
    "the stale mapping reads zeros, never the freed tail ({:?})",
    &tail[..8]
  );
  store.dax_write_raw(&m, 200, &[0xEE; 100]);
  assert_eq!(
    volume.stat(&store, f).unwrap().size,
    100,
    "the file stays the truncated size; the stale write went nowhere"
  );
}

/// §4.6, AC-4.12 (teardown on write, the sibling of truncate): a read-only mapping held while the daemon writes the
/// same inode — a concurrent NFS/FUSE WRITE, or a landing/snapshot copy-up — is revoked before the write changes the
/// block, so the guest's stale window never serves a torn or pre-write view, nor a block the write may have moved
/// and freed. Map a file read-only, write it through the daemon, then drive the hardware path via the STALE mapping:
/// it is no longer live and its read is zeros, while the daemon path shows the new bytes.
#[test]
fn a_daemon_write_while_mapped_revokes_the_read_window() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let m = volume.dax_map_read(&mut store, f).unwrap();
  assert!(store.dax_is_live(&m), "the read mapping starts live");

  volume.write(&mut store, f, 0, &[0x22; 64]).unwrap();
  assert!(
    !store.dax_is_live(&m),
    "the daemon write revoked the held read mapping"
  );

  assert!(
    store.dax_read_raw(&m, 0, 64).iter().all(|&b| b == 0),
    "the stale read window is inert after the revoke"
  );
  let mut now = vec![0u8; 4];
  volume.read(&store, f, 0, &mut now).unwrap();
  assert_eq!(&now, &[0x22; 4], "the daemon path shows the written bytes");
}

/// §4.6, AC-4.12 (cross-volume clone share under DAX): two volumes sharing a snapshot's blocks via `clone_of` are
/// isolated under a writable mapping — mapping a shared file writable copies it up through the write boundary (CoW),
/// so the mapping is on this volume's own private block, and a write through it never reaches the other volume's
/// shared copy. Origin writes a page of `0x41` and snapshots; a clone shares the block; the clone maps the file
/// writable and writes "BBB" the hardware way; the origin still reads `0x41`, the clone its own "BBB". (A mappable
/// file is a single content block, not a tiny inline file, so the page size matters.)
#[test]
fn a_writable_map_in_a_clone_copies_up_and_spares_the_shared_origin() {
  let mut store = store();
  let mut origin = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut origin, "f", 0x41, DAX_PAGE);
  let snap = origin.snapshot(&mut store).unwrap();
  let mut clone = Volume::clone_of(&store, &mut origin, snap, clone_config(9)).unwrap();
  clone.enable_writable_dax(); // a clone does not inherit the opt-in; its own owner enables it (L0, per-volume).

  let clone_root = clone.root_inode(&store).unwrap();
  let cf = clone.lookup_no(&store, clone_root, "f").unwrap().inode;
  let m = clone.dax_map_write(&mut store, cf).unwrap();
  store.dax_write_raw(&m, 0, b"BBB");

  let mut origin_bytes = vec![0u8; 3];
  origin.read(&store, f, 0, &mut origin_bytes).unwrap();
  assert_eq!(
    &origin_bytes, &[0x41; 3],
    "the write through the clone's DAX mapping spared the shared origin block"
  );
  let mut clone_bytes = vec![0u8; 3];
  clone.read(&store, cf, 0, &mut clone_bytes).unwrap();
  assert_eq!(
    &clone_bytes, b"BBB",
    "the clone sees its own copied-up write"
  );
}

/// §4.6 (the raw hardware path bounds a colossal offset or length too, not only the daemon-mediated path): a guest's
/// page table can present any offset or length, so `dax_read_raw`/`dax_write_raw` clamp to the mapped page span
/// (saturating), making a colossal offset an empty access and a colossal length a span-bounded one — never an
/// out-of-bounds read, an overflow, an unbounded allocation, or a panic.
#[test]
fn the_raw_hardware_path_bounds_a_colossal_offset_or_length() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  let m = volume.dax_map_write(&mut store, f).unwrap();
  assert!(
    store.dax_read_raw(&m, usize::MAX, 100).is_empty(),
    "a colossal offset reads nothing, clamped to the span"
  );
  assert!(
    store.dax_read_raw(&m, 0, usize::MAX).len() <= m.page_span(),
    "a colossal length clamps to the mapped span, never an unbounded allocation"
  );
  store.dax_write_raw(&m, usize::MAX, &[0xEE; 100]);
  assert!(
    store.dax_read_raw(&m, 0, 4).iter().all(|&b| b == 0x11),
    "the out-of-range write touched nothing"
  );
}

// ───────────────────────────────────────────────────────────── L2 the moving target (stripe-shift migration)

/// Whether every byte of `bytes` equals `v` — keeps the assert closures out of the test bodies (the complexity budget).
fn all_bytes(bytes: &[u8], v: u8) -> bool {
  bytes.iter().all(|&b| b == v)
}

/// The first shift of a test that needs a §4.6 L1 sandbox, which is also the question "can this host back one".
///
/// `None`, with the skip printed, where the host declined the huge page the sandbox needs — every non-Linux host
/// and a Linux without transparent huge pages always will, and a Linux *with* them may decline any individual
/// span, because a transparent huge page is granted best-effort out of whatever contiguous memory the kernel has
/// at that moment. So the question is asked by the test's own first shift rather than by a separate probe: a probe
/// would both race the answer and consume a huge page of the very memory it was asking about.
fn first_shift(
  store: &mut Store,
  volume: &mut Volume,
  mapping: &Mapping,
  test: &str,
) -> Option<Mapping> {
  match volume.dax_migrate(store, mapping) {
    Ok(moved) => Some(moved),
    Err(VfsError::NoPhysicalIsolation) => {
      println!(
        "SKIP {test}: this host backed no transparent huge page for the span, so §4.6's L1 sandbox cannot be reserved"
      );
      None
    }
    Err(other) => panic!("a shift is either performed or refused for want of a sandbox: {other:?}"),
  }
}

/// §4.6 multi-tenancy **L2** (the moving target; the `dax_migrate` stripe-shift primitive the periodic Rowhammer
/// defence composes from). Do: map a file writable, write a known marker the hardware way, migrate it. Expect: the
/// content moves to a *physically different* block with every byte preserved; the inode follows, so the daemon path
/// and a plain non-DAX read both reach the moved bytes; the new mapping is live, the old is inert; the counter moved.
#[test]
fn a_migration_moves_the_content_to_a_new_block_and_repoints_the_inode() {
  let mut store = store_sandboxed();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let m = volume.dax_map_write(&mut store, f).unwrap();
  store.dax_write_raw(&m, 0, &[0x55; 8]); // a known marker written through the guest's own page table

  let Some(m2) = first_shift(
    &mut store,
    &mut volume,
    &m,
    "a_migration_moves_the_content_to_a_new_block_and_repoints_the_inode",
  ) else {
    return;
  };

  assert_ne!(
    m2.block, m.block,
    "the content moved to a physically different block (the moving target)"
  );
  assert!(store.dax_is_live(&m2), "the new mapping is live");
  assert!(
    !store.dax_is_live(&m),
    "the old mapping is inert (revoked-pending)"
  );
  assert_eq!(
    store.dax.migrated(),
    1,
    "the migration counter moved (non-vacuity)"
  );
  // Every byte survived the shift: the marker and the file's body.
  assert_eq!(
    store.dax_read_raw(&m2, 0, 8),
    vec![0x55; 8],
    "the written marker moved with the content"
  );
  assert!(
    all_bytes(&store.dax_read_raw(&m2, 8, 16), 0x41),
    "the rest of the file moved intact"
  );
  // The inode follows the move, so the daemon path and a plain non-DAX read reach the moved bytes, not a stale block.
  assert_eq!(
    volume.dax_read(&store, &m2, 0, 8).unwrap(),
    vec![0x55; 8],
    "the daemon path sees the moved block"
  );
  let mut buf = [0u8; 8];
  volume.read(&store, f, 0, &mut buf).unwrap();
  assert_eq!(
    &buf, &[0x55; 8],
    "a non-DAX reader reaches the moved content"
  );
}

/// §4.6 L2 (the stale-TLB hold). Do: migrate a writable mapping, then release the old mapping as the device would
/// after confirming the window unmap. Expect: the old window is inert immediately, but the old block stays **pinned**
/// (its free deferred, its bytes kept) across the migrate, and is unpinned — reusable by a neighbour — only after the
/// release. This is why teardown holds the pin across the unmap: a neighbour can never take the block while the
/// guest's possibly-stale page table still maps it.
#[test]
fn a_migration_holds_the_old_block_pinned_until_the_release() {
  let mut store = store_sandboxed();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let m = volume.dax_map_write(&mut store, f).unwrap();
  let old_block = m.block;

  if first_shift(
    &mut store,
    &mut volume,
    &m,
    "a_migration_holds_the_old_block_pinned_until_the_release",
  )
  .is_none()
  {
    return;
  }

  assert!(
    all_bytes(&store.dax_read_raw(&m, 0, 8), 0),
    "the old window is inert after the migrate"
  );
  assert!(
    store.content.arena().is_pinned(old_block),
    "the old block is still pinned — the stale-TLB hold"
  );
  assert!(
    store.dax_release_after_unmap(&m),
    "the release after the confirmed unmap drops the hold"
  );
  assert!(
    !store.content.arena().is_pinned(old_block),
    "the old block is unpinned and reusable only after the release"
  );
}

/// §4.6 L2. Do: migrate a *read-only* mapping. Expect: refused `Invalid` — the moving target is for writable blocks a
/// guest can hammer; read-only DAX is immutable shared content a guest cannot hammer into a neighbour, and moving it
/// would break the share. Non-vacuity: a writable mapping of the same file migrates.
#[test]
fn a_read_only_mapping_is_not_migrated() {
  let mut store = store_sandboxed();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);

  let ro = volume.dax_map_read(&mut store, f).unwrap();
  assert!(
    matches!(volume.dax_migrate(&mut store, &ro), Err(VfsError::Invalid)),
    "a read-only mapping is not migrated"
  );
  assert_eq!(store.dax.migrated(), 0, "nothing migrated");

  // Non-vacuity: the same file, mapped writable, does migrate — where the host can back the sandbox it needs.
  store.dax_revoke(&ro);
  let rw = volume.dax_map_write(&mut store, f).unwrap();
  if first_shift(
    &mut store,
    &mut volume,
    &rw,
    "a_read_only_mapping_is_not_migrated (the non-vacuity leg)",
  )
  .is_none()
  {
    return;
  }
  assert_eq!(store.dax.migrated(), 1);
}

/// §4.6 L2 (the stripe-shift placement, [`stripe_target`]). Do: over a seeded generator, draw many targets for every
/// slot of an 8-slot sandbox. Expect: every target is inside the sandbox `[0, 8)` and never the current slot (a shift
/// must move), and every slot is reachable as a target over the sweep (non-vacuity — the placement is not stuck); a
/// one-slot sandbox returns its only slot, signalling no shift is available. This is the bounded, no-quarantine
/// placement: the content walks a capped physical sandbox, never a growing quarantine pool.
#[test]
fn the_stripe_target_stays_in_the_sandbox_and_always_moves() {
  use core::num::NonZeroUsize;
  use slates_machine::stats::Xorshift;
  let mut rng = Xorshift::new(0x0BAD_F00D_1234_5678);
  let slots = NonZeroUsize::new(8).unwrap();
  let mut seen = [false; 8];
  for current in 0..8usize {
    for _ in 0..2000 {
      let t = stripe_target(slots, current, &mut rng);
      assert!(t < 8, "the target stays strictly inside the sandbox");
      assert_ne!(t, current, "a shift always moves to a different slot");
      seen[t] = true;
    }
  }
  assert!(
    seen.iter().all(|&s| s),
    "every slot in the sandbox is reachable (the placement is not stuck)"
  );
  assert_eq!(
    stripe_target(NonZeroUsize::new(1).unwrap(), 0, &mut rng),
    0,
    "a one-slot sandbox cannot move: it returns its only slot (no shift available)"
  );
}

/// §4.6 **L1** (zones and guard rows), the leg every host runs. Do: ask for a shift on a store whose arena the
/// host may or may not have backed with a huge page. Expect: on a host that cannot back one, the shift is
/// **refused** `NoPhysicalIsolation` and nothing moved — a shift inside a 4 KiB-paged region is a virtual move the
/// kernel may back with the very row it came from, which §4.6 calls security theatre, and slates refuses rather
/// than performs it; on a host that can, the shift succeeds and the volume holds a sandbox. One test, both legs,
/// so neither host silently loses the rule.
#[test]
fn a_shift_is_refused_where_the_host_cannot_make_it_move_physical_rows() {
  let mut store = store_sandboxed();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let m = volume.dax_map_write(&mut store, f).unwrap();
  let before = m.block;

  match volume.dax_migrate(&mut store, &m) {
    Err(VfsError::NoPhysicalIsolation) => {
      assert_eq!(store.dax.migrated(), 0, "nothing migrated");
      assert!(
        store.dax_is_live(&m),
        "the refused shift left the window exactly as it was"
      );
      assert!(
        store.dax_zones.is_empty(),
        "no sandbox was left half-reserved by the refusal"
      );
      assert!(
        all_bytes(&store.dax_read_raw(&m, 0, 8), 0x41),
        "the content is untouched where it was"
      );
      println!(
        "this host backs no transparent huge page: the shift is refused, which is the rule under test"
      );
    }
    Ok(moved) => {
      assert_ne!(moved.block, before, "the shift moved the content");
      assert_eq!(
        store.dax_zones.len(),
        1,
        "the volume holds one sandbox (non-vacuity: the shift really drew from a zone)"
      );
    }
    other => panic!("a shift is either refused for want of a sandbox or performed: {other:?}"),
  }
}

/// §4.6 L1 (the sandbox's shape). Do: on a host that backs one, shift a tenant's writable content many times over.
/// Expect: every landing is inside that tenant's own sandbox, never on a guard row at either end, the content is
/// byte-identical after each one, and over the sweep more than one row is used (non-vacuity — the placement is not
/// stuck on a single slot, which would be a sandbox that does not move).
#[test]
fn every_shift_lands_inside_the_tenants_sandbox_and_never_on_a_guard_row() {
  let mut store = store_sandboxed();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let mut m = volume.dax_map_write(&mut store, f).unwrap();
  store.dax_write_raw(&m, 0, &[0x55; 8]);

  // Derived: enough shifts that a stuck placement cannot pass by luck, and few enough that the test stays a
  // fraction of a second — the zone has tens of rows, so twenty draws visit several of them with overwhelming
  // probability while costing twenty 64 KiB copies.
  const SHIFTS: usize = 20;
  let mut landed = Vec::new();
  for shift in 0..SHIFTS {
    // The first draw is the one that reserves the sandbox, so it is the one that answers "can this host".
    let moved = match first_shift(
      &mut store,
      &mut volume,
      &m,
      "every_shift_lands_inside_the_tenants_sandbox_and_never_on_a_guard_row",
    ) {
      Some(moved) => moved,
      None if shift == 0 => return,
      None => panic!("the sandbox was reserved on the first shift and must still be there"),
    };
    let (_, slots) = store.dax_zones.of_volume(f.prefix()).unwrap().held();
    let slot = store
      .dax_zones
      .of_volume(f.prefix())
      .unwrap()
      .slot_of(moved.block)
      .expect("the content landed in the tenant's own sandbox");
    assert_ne!(slot, 0, "a landing never takes the leading guard row");
    assert_ne!(
      slot,
      slots.saturating_sub(1),
      "a landing never takes the trailing guard row"
    );
    assert_eq!(
      store.dax_read_raw(&moved, 0, 8),
      vec![0x55; 8],
      "the marker survived the shift"
    );
    assert!(
      all_bytes(&store.dax_read_raw(&moved, 8, 16), 0x41),
      "the rest of the file survived the shift"
    );
    store.dax_release_after_unmap(&m);
    landed.push(slot);
    m = moved;
  }
  landed.sort_unstable();
  landed.dedup();
  assert!(
    landed.len() > 1,
    "the placement visits more than one row over {SHIFTS} shifts (it is not stuck): {landed:?}"
  );
}

/// §4.6 L1 (the isolation the zone exists for). Do: on a host that backs sandboxes, give two volumes on the same
/// shard a writable DAX block each and shift both many times. Expect: no row either tenant ever lands on is a row
/// the other's sandbox covers — the co-location that makes a neighbour's hammer reach this tenant's bits cannot
/// happen, because each tenant's rows are inside its own huge page with guard rows at the ends.
#[test]
fn two_tenants_never_land_on_a_row_inside_the_others_sandbox() {
  let mut store = store_sandboxed();
  let mut one = volume(&mut store, 1 << 20);
  // A second volume on the same shard arena, with its own prefix — the fixture's `volume` always makes prefix 7,
  // so two of them would be one volume created twice, not two tenants.
  let mut two = Volume::create(&mut store, clone_config(9)).unwrap();
  two.enable_writable_dax();
  let a = file_of(&mut store, &mut one, "a", 0xA1, DAX_PAGE);
  let b = file_of(&mut store, &mut two, "b", 0xB2, DAX_PAGE);
  let mut ma = one.dax_map_write(&mut store, a).unwrap();
  let mut mb = two.dax_map_write(&mut store, b).unwrap();

  // Derived: as above — enough draws that a placement that strayed would be caught, cheap enough to stay fast.
  const SHIFTS: usize = 12;
  for shift in 0..SHIFTS {
    let test = "two_tenants_never_land_on_a_row_inside_the_others_sandbox";
    let (Some(moved_a), Some(moved_b)) = (
      first_shift(&mut store, &mut one, &ma, test),
      first_shift(&mut store, &mut two, &mb, test),
    ) else {
      assert_eq!(shift, 0, "both sandboxes were reserved on the first shift");
      return;
    };
    let zone_a = store.dax_zones.of_volume(a.prefix()).unwrap();
    let zone_b = store.dax_zones.of_volume(b.prefix()).unwrap();
    assert!(
      zone_a.slot_of(moved_a.block).is_some() && zone_a.slot_of(moved_b.block).is_none(),
      "the first tenant landed in its own sandbox and the second never did"
    );
    assert!(
      zone_b.slot_of(moved_b.block).is_some() && zone_b.slot_of(moved_a.block).is_none(),
      "the second tenant landed in its own sandbox and the first never did"
    );
    // Each tenant's bytes are its own after the shift, which is the leak this isolation is about.
    assert!(
      all_bytes(&store.dax_read_raw(&moved_a, 8, 16), 0xA1),
      "the first tenant's content is its own"
    );
    assert!(
      all_bytes(&store.dax_read_raw(&moved_b, 8, 16), 0xB2),
      "the second tenant's content is its own"
    );
    store.dax_release_after_unmap(&ma);
    store.dax_release_after_unmap(&mb);
    ma = moved_a;
    mb = moved_b;
  }
  assert_eq!(store.dax_zones.len(), 2, "each tenant has its own sandbox");
}

/// §4.6 L1 (the sandbox's own lifecycle). Do: on a host that backs sandboxes, give a volume a writable window
/// so it reserves one, then destroy the volume. Expect: the sandbox's huge page goes back to the arena — the
/// shard's allocated bytes return to what they were before the tenant existed, so a sandbox is a cost that
/// ends with its tenant and not a page the shard loses for the rest of its life.
///
/// The measurement is taken **before the map**, because the map is what reserves the sandbox now: a window is
/// placed in its tenant's rows from the first byte rather than from the first shift
/// (`a_writable_window_is_placed_in_its_sandbox_before_any_shift`). Measuring after the map was this test's
/// own bug — it then saw the shift add nothing, which is exactly right and read as a failure.
#[test]
fn destroying_a_volume_gives_its_sandbox_back_to_the_arena() {
  let mut store = store_sandboxed();
  let mut volume = volume(&mut store, 1 << 20);
  let prefix = volume.prefix();
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let allocated_before = store.content.arena().allocated_bytes();
  let m = volume.dax_map_write(&mut store, f).unwrap();

  let Some(moved) = first_shift(
    &mut store,
    &mut volume,
    &m,
    "destroying_a_volume_gives_its_sandbox_back_to_the_arena",
  ) else {
    return;
  };
  store.dax_release_after_unmap(&m);
  let with_sandbox = store.content.arena().allocated_bytes();
  assert!(
    with_sandbox > allocated_before,
    "the sandbox took a huge page from the arena ({allocated_before} -> {with_sandbox})"
  );

  assert_eq!(store.dax_zones.len(), 1);

  store.dax_release_after_unmap(&moved);
  store.dax_revoke_volume(prefix);
  assert_eq!(
    store.dax_zones.len(),
    0,
    "the destroyed volume's sandbox is gone"
  );
  let after_destroy = store.content.arena().allocated_bytes();
  assert!(
    after_destroy < with_sandbox,
    "the sandbox's rows went back to the arena ({with_sandbox} -> {after_destroy})"
  );
  // What the destroy gives back is the sandbox: every row the zone was holding. What stays is the tenant's
  // own content, at **row granularity** — a file placed in a sandbox occupies a whole row, because a row
  // shared with another file is precisely the co-location the sandbox exists to prevent. So the page comes
  // back *minus that row*, and the shard is left holding one row per placed file more than before the tenant
  // existed: the price of the guarantee, not a leak. This bound says both halves at once, which the two
  // over-claims it replaced — "no worse off than before" and "a whole huge page came back" — did not.
  assert!(
    after_destroy <= allocated_before.saturating_add(store.content.chunk_bytes()),
    "and what stays is the tenant's own content at row granularity, nothing more      ({allocated_before} -> {after_destroy})"
  );
}

/// §4.6 L1 over the backing a **daemon actually uses**. Do: build a store whose content arena is a shared memory
/// object — what the production arena is (A-64: content lives in the anchor's object, so it survives a restart),
/// not the private mapping the other sandbox tests map — and shift a tenant's writable content in it. Expect: the
/// sandbox is reserved inside the object's own pages and the shift lands in it, so the guest's DAX window, which
/// is a range of that object's descriptor, is the thing being protected.
///
/// This is a separate test because it is a separate kernel question: a private mapping's huge pages are governed
/// by `transparent_hugepage/enabled` and a shared object's by `transparent_hugepage/shmem_enabled`, and a host can
/// have the first on and the second off — measured on Fedora 6.19, which is `[madvise]` for one and `[never]` for
/// the other. A fixture that only mapped privately would pass on such a host while the daemon got no sandbox at
/// all, which is the trap of testing a backing the product does not use.
#[test]
fn a_sandbox_is_reserved_in_the_shared_object_a_daemon_serves_dax_from() {
  use slates_mem::arena::ChunkArena;
  use slates_mem::region::Region;
  use slates_mem::shared::{ExclusiveObject, SharedObject};
  use slates_mem::words::Words;
  use slates_vfs::volume::StoreConfig;

  let page = 4096usize;
  // Derived: as the private fixture — eight of the host's huge pages, so the sandbox, the content and the arena's
  // rounding all fit without the fixture's size shaping the result.
  const SANDBOX_HUGE_PAGES: usize = 8;
  let Some(huge) =
    slates_machine::facts::huge_page_bytes().and_then(|bytes| usize::try_from(bytes).ok())
  else {
    println!(
      "SKIP a_sandbox_is_reserved_in_the_shared_object_a_daemon_serves_dax_from: this host has no transparent huge page"
    );
    return;
  };
  let object = SharedObject::create(
    &format!("sl-dax-zone-{}", std::process::id()),
    huge * SANDBOX_HUGE_PAGES,
    Words::new(),
  )
  .unwrap();
  let mut arena = ChunkArena::new(page);
  // SAFETY: this test is the object's only mapper, and it declares no words.
  arena
    .add_region(Region::shared(
      unsafe { ExclusiveObject::new(object) },
      page,
    ))
    .unwrap();
  let mut store = Store::new(
    &StoreConfig {
      page,
      cache_line: 128,
      max_dirs: 1 << 10,
      max_inodes: 1 << 12,
      max_chunks: 1 << 12,
      max_dir_blocks: 1 << 10,
      dir_cutover: 16,
    },
    arena,
    0,
  );
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let m = volume.dax_map_write(&mut store, f).unwrap();
  store.dax_write_raw(&m, 0, &[0x55; 8]);

  let Some(moved) = first_shift(
    &mut store,
    &mut volume,
    &m,
    "a_sandbox_is_reserved_in_the_shared_object_a_daemon_serves_dax_from",
  ) else {
    return;
  };

  let zone = store.dax_zones.of_volume(f.prefix()).unwrap();
  assert!(
    zone.slot_of(moved.block).is_some(),
    "the content landed in the sandbox reserved inside the shared object"
  );
  assert_eq!(
    store.dax_read_raw(&moved, 0, 8),
    vec![0x55; 8],
    "the marker moved with it"
  );
  // The window a device would hand a VMM is a range of this object's descriptor, and it names the moved block.
  #[cfg(unix)]
  {
    let (_, offset, len) = store
      .dax_backing(&moved)
      .expect("the moved block is mappable");
    assert_eq!(offset, moved.block.offset(), "the window names the new row");
    assert!(len >= DAX_PAGE, "the window covers the file's page");
  }
}

/// One whole round of the periodic shift, driven in slices as the daemon's loop drives it: the windows shifted
/// and the slices it took, or `None` with the skip printed where this host backs no sandbox. The budget is one
/// byte, so a slice stops after the first window it copies and the round's slicing is what is under test.
fn run_one_shift_round(
  store: &mut Store,
  volume: &mut Volume,
  due: &[Mapping],
) -> Option<(u64, usize)> {
  let mut at = 0usize;
  let mut slices = 0usize;
  let mut shifted = 0u64;
  while at < due.len() {
    let slice = volume.dax_shift_slice(store, due, at, 1);
    if slice.unsupported {
      println!(
        "SKIP one_round_of_the_periodic_shift_visits_every_window_once_and_slices: this host backed no \
         transparent huge page, so §4.6's L1 sandbox cannot be reserved"
      );
      return None;
    }
    assert!(
      slice.next > at,
      "a slice always advances, so a round terminates"
    );
    at = slice.next;
    shifted = shifted.saturating_add(slice.shifted);
    slices = slices.saturating_add(1);
  }
  Some((shifted, slices))
}

/// §4.6 **L2's periodic shift, one round** ([`Volume::dax_shift_slice`]) — the unit the daemon's cadence drives.
/// Do: give a volume three writable DAX windows and run one round in slices with a budget far smaller than the
/// round, as a shard does to stay responsive. Expect: every window is visited exactly once (the snapshot is what
/// makes that true — a re-read list would offer each shifted window again and the sweep would chase its own
/// tail), every slice advances, each landing is inside the tenant's sandbox, and the round shifted all three
/// (non-vacuity: a sweep that ran but moved nothing reads as zero). Where the host backs no sandbox the round
/// says so at once instead of refusing window by window, which is what the daemon's loop reads as "not here".
#[test]
fn one_round_of_the_periodic_shift_visits_every_window_once_and_slices() {
  let mut store = store_sandboxed();
  let mut volume = volume(&mut store, 1 << 22);
  // Derived: three windows is the fewest that distinguishes "sliced" from "one slice did everything" while
  // keeping the round's cost to three block copies.
  const WINDOWS: usize = 3;
  let mut mapped = Vec::new();
  for which in 0..WINDOWS {
    let name = format!("f{which}");
    let f = file_of(&mut store, &mut volume, &name, 0x41, DAX_PAGE);
    mapped.push(volume.dax_map_write(&mut store, f).unwrap());
  }
  let prefix = volume.prefix();
  let due = store.dax.writable_of_prefix(prefix);
  assert_eq!(due.len(), WINDOWS, "three live writable windows to shift");

  // A budget of one byte, so each slice stops after the first window it copies: the round must take three.
  let Some((shifted, slices)) = run_one_shift_round(&mut store, &mut volume, &due) else {
    return;
  };
  assert_eq!(
    shifted, WINDOWS as u64,
    "every window was shifted exactly once"
  );
  assert_eq!(
    slices, WINDOWS,
    "the budget sliced the round, one window each"
  );
  assert_eq!(
    store.dax.migrated(),
    WINDOWS as u64,
    "the registry counted the same shifts (non-vacuity)"
  );

  // Every window now sits on a row of this tenant's own sandbox, and none on the row it left.
  let after = store.dax.writable_of_prefix(prefix);
  assert_eq!(after.len(), WINDOWS, "each window was replaced, not lost");
  let zone = store.dax_zones.of_volume(prefix).unwrap();
  for moved in &after {
    assert!(
      zone.slot_of(moved.block).is_some(),
      "a shifted window sits in the tenant's sandbox"
    );
    // Its *own* previous row, not any window's: since a writable map is now placed in the sandbox from the
    // start, the rows the three windows began on are all inside it, and one window landing on a row another
    // has just vacated is a correct draw, not a failure to move.
    let was = mapped
      .iter()
      .find(|before| before.inode == moved.inode)
      .map(|before| before.block);
    assert_ne!(
      Some(moved.block),
      was,
      "a shifted window is not on the row it left"
    );
  }
}

/// §4.6 **L1 under attack: the row that leaves the sandbox.** A zone holds every slot it is not lending, so a
/// neighbour can never hold a row inside this tenant's huge page — but the row the *tenant* holds is the
/// inode's, and the ordinary content path frees it on a delete or truncate, by a route the zone does not sit
/// on. From that moment the row is in the general free lists and a neighbour can take it, which is exactly the
/// co-location the sandbox exists to prevent.
///
/// Do: shift a tenant's content into its sandbox, delete the file so the row it sat on goes back to the
/// allocator, then let a second volume allocate until it holds a row inside the first tenant's page. Map and
/// shift the first tenant again. Expect: the shift never lands on the row the neighbour holds — the zone
/// re-claims what it can before it draws, and leaves out what it cannot — and the neighbour's bytes are
/// untouched by the tenant's writes.
#[test]
fn a_row_a_neighbour_took_is_never_drawn_for_the_tenant_again() {
  let mut store = store_sandboxed();
  let mut tenant = volume(&mut store, 1 << 22);
  let mut neighbour = Volume::create(&mut store, clone_config(9)).unwrap();
  let f = file_of(&mut store, &mut tenant, "f", 0x41, DAX_PAGE);
  let m = tenant.dax_map_write(&mut store, f).unwrap();
  let Some(moved) = first_shift(
    &mut store,
    &mut tenant,
    &m,
    "a_row_a_neighbour_took_is_never_drawn_for_the_tenant_again",
  ) else {
    return;
  };
  store.dax_release_after_unmap(&m);
  let prefix = tenant.prefix();
  let (held_before, slots) = store.dax_zones.of_volume(prefix).unwrap().held();
  assert_eq!(
    held_before,
    slots - 1,
    "the zone holds every row but the one the tenant is on"
  );

  // The tenant's own ordinary path frees its row: the window is revoked and the file deleted.
  store.dax_release_after_unmap(&moved);
  let root = tenant.root_inode(&store).unwrap();
  tenant.unlink_no(&mut store, root, "f").unwrap();

  // The neighbour allocates until it holds a row inside the tenant's page. Bounded: a handful of blocks of the
  // tenant's slot size is enough for the allocator to hand back the row just freed, and the test says so rather
  // than looping unbounded if it does not.
  const TRIES: usize = 16;
  let mut taken = None;
  for which in 0..TRIES {
    let name = format!("n{which}");
    let slot_bytes = store.dax_zones.of_volume(prefix).unwrap().slot_bytes();
    let n = file_of(&mut store, &mut neighbour, &name, 0xB2, slot_bytes);
    // A read map is how a test learns which block an inode's content sits on; it is revoked at once, so the
    // block stays the neighbour's *allocation* without a pin doing the work this test means to attribute to
    // the zone.
    let probe = neighbour.dax_map_read(&mut store, n).unwrap();
    let block = probe.block;
    store.dax_revoke(&probe);
    if let Some(index) = store.dax_zones.of_volume(prefix).unwrap().slot_of(block) {
      taken = Some((index, block));
      break;
    }
  }
  let Some((stolen, stolen_block)) = taken else {
    println!(
      "SKIP a_row_a_neighbour_took_is_never_drawn_for_the_tenant_again: the allocator did not hand a row of \
       the tenant's page to the neighbour in {TRIES} tries, so there is nothing to attack here"
    );
    return;
  };

  // The tenant comes back and shifts again, many times. It must never be given the row the neighbour holds.
  let f2 = file_of(&mut store, &mut tenant, "f2", 0x41, DAX_PAGE);
  let mut live = tenant.dax_map_write(&mut store, f2).unwrap();
  // Derived: enough draws that a placement that could pick the stolen row would, over a sandbox of tens of
  // rows, almost surely have done so.
  const DRAWS: usize = 30;
  for _ in 0..DRAWS {
    let Ok(next) = tenant.dax_migrate(&mut store, &live) else {
      break;
    };
    let zone = store.dax_zones.of_volume(prefix).unwrap();
    assert_ne!(
      zone.slot_of(next.block),
      Some(stolen),
      "the tenant was never drawn onto the row its neighbour holds"
    );
    assert_ne!(
      next.block, stolen_block,
      "and never onto that block by any other name"
    );
    store.dax_release_after_unmap(&live);
    live = next;
  }
  // The neighbour's bytes are its own throughout: the tenant wrote its own rows, never that one.
  assert!(
    all_bytes(
      &store.content.arena().bytes(stolen_block).unwrap()[..16],
      0xB2
    ),
    "the neighbour's content was never written by the tenant"
  );
}

/// §4.6 L1 (the cost of hearing "no"). A refused sandbox reservation allocates a huge page's span, discards it,
/// walks `/proc/self/smaps` three times — a walk of the whole address space each — faults the span and frees
/// it: **measured 53 µs on fedora 6.19.10**. The shift loop asks every 16 ms per window, so asking again each
/// time would burn 0.33% of a core per window to re-learn the same answer, and more as the daemon's address
/// space grows.
///
/// Do: ask for a shift on this host and see which answer it gives. Expect: where the host refuses, the volume
/// is remembered as refused so the question is not asked again, and a re-arm forgets it so a host that frees
/// memory later is asked once more; where the host backs a sandbox, nothing is remembered. One test, both
/// legs, so neither host silently loses the rule.
#[test]
fn a_refused_sandbox_is_remembered_until_the_caller_re_arms() {
  let mut store = store_sandboxed();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  assert_eq!(
    store.dax_zones.refused_len(),
    0,
    "nothing is remembered before the question is asked"
  );
  // The map asks the question too, since a writable window is placed in its sandbox from the first byte, so
  // the memo may already hold this volume by the time the shift runs. Checked above rather than below for
  // exactly that reason.
  let m = volume.dax_map_write(&mut store, f).unwrap();

  match volume.dax_migrate(&mut store, &m) {
    Err(VfsError::NoPhysicalIsolation) => {
      // Two ways to be refused, and only one of them costs anything to re-learn. A host with no transparent
      // huge page at all answers from one `Option` and needs no memo; a host that *has* one and declined this
      // span is the 53 µs case, and that is the one that must be remembered.
      let host_has_huge_pages = slates_machine::facts::huge_page_bytes().is_some();
      let remembered = usize::from(host_has_huge_pages);
      assert_eq!(
        store.dax_zones.refused_len(),
        remembered,
        "a host that declined a span it could have backed is remembered; one with no huge page needs no memo"
      );
      // The answer does not change, and asking again does not re-ask the host.
      assert!(
        matches!(
          volume.dax_migrate(&mut store, &m),
          Err(VfsError::NoPhysicalIsolation)
        ),
        "the memo gives the same answer it memorised"
      );
      assert_eq!(
        store.dax_zones.refused_len(),
        remembered,
        "and asking again added nothing"
      );
      store.dax_zones.rearm_refusals();
      assert_eq!(
        store.dax_zones.refused_len(),
        0,
        "the re-arm forgets it, so a host that frees memory is asked once more"
      );
    }
    Ok(_) => {
      assert_eq!(
        store.dax_zones.refused_len(),
        0,
        "a host that backs a sandbox remembers no refusal"
      );
      assert_eq!(store.dax_zones.len(), 1, "it has a sandbox instead");
    }
    other => panic!("a shift is performed or refused for want of a sandbox: {other:?}"),
  }
}

/// §4.6 B2, the **second** way a guard outlives what it describes — found by the chaos property *after* the
/// pin check was already in, which is why the pin check alone was not the fix. A guard can name a block that is
/// live in the registry and still pinned, yet no longer the content of the inode it was taken for: the inode is
/// gone, or its content moved. The bytes under that guard are not the file's any more — once the block is
/// re-allocated they are another file's — and healing there writes a dead file's content over a live one's.
///
/// Do: guard a read map of a file, unlink the file so the mapping is no longer current for any inode while its
/// own window keeps the block pinned, then change the block's bytes as a hammer would. Run the scrub. Expect:
/// the stale guard is **dropped** and nothing is healed, even though the mapping is live and the block is
/// pinned — the two conditions the earlier fix checked.
#[test]
fn the_net_drops_a_guard_whose_mapping_is_no_longer_its_inodes_content() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  volume.enable_dax_integrity_guard();
  const FILL: u8 = 0x41;

  let no = file_of(&mut store, &mut volume, "dead", FILL, DAX_PAGE);
  let stale = volume.dax_map_read(&mut store, no).unwrap();
  assert_eq!(store.dax.guards(), 1, "the read map is guarded");

  // The file goes, but its own window still holds the block, so the two conditions the earlier fix checks
  // both still hold — and only the currency check can tell that the guard describes nothing.
  let root = volume.root_inode(&store).unwrap();
  volume.unlink_no(&mut store, root, "dead").unwrap();
  assert!(
    store.dax.is_live(&stale),
    "the mapping is still in the registry"
  );
  assert!(
    store.content.arena().is_pinned(stale.block),
    "and its block is still pinned, so the pin check would pass"
  );
  assert!(
    !volume.dax_still_current(&store, &stale),
    "but it is no longer any inode's content: this is the condition under test"
  );

  // Change the bytes, as a hammer or a re-allocation would. A net that acted here would write the dead file's
  // content back over them.
  flip_a_byte(&mut store, stale.block);
  let changed = store
    .content
    .arena()
    .bytes(stale.block)
    .map(<[u8]>::to_vec)
    .unwrap_or_default();

  let slice = volume.dax_scrub_slice(&mut store, 0, u64::MAX);

  assert_eq!(
    slice.healed, 0,
    "the net wrote a dead file's content back over a block it no longer describes"
  );
  assert_eq!(
    slice.dropped, 1,
    "the stale guard was dropped instead (non-vacuity: the pass reached it)"
  );
  assert_eq!(store.dax.guards(), 0, "and its clean copy went with it");
  assert_eq!(
    store.content.arena().bytes(stale.block).map(<[u8]>::to_vec),
    Some(changed),
    "the block's bytes are exactly as they were left"
  );
}

/// §4.6 B2, the bug the chaos property found
/// (`docs/bugs/2026-10-19-the-self-healing-net-resurrects-a-deleted-files-plaintext.md`). The net must never
/// write through a mapping whose block is no longer pinned: the block may already have been released *and
/// scrubbed*, and "healing" it puts a deleted file's plaintext back into memory the allocator has freed — the
/// cross-tenant disclosure the gate exists to prevent, performed by the gate's own net.
///
/// Do: map one file both writable and read-only (the read map is guarded), unlink it, then release the windows
/// so the deferred free completes and the block is scrubbed — the state the shared per-block pin lets happen
/// while the read mapping is still in the registry — and run the scrub. Expect: the pass **drops** the stale
/// guard and heals nothing, so the freed block keeps the zeroes the free wrote into it.
///
/// Kept as a named case rather than a proptest seed because it is one exact interleaving with one exact
/// outcome, and a reader should be able to see it without replaying a hash.
#[test]
fn the_net_never_heals_through_a_block_the_arena_has_freed() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  volume.enable_dax_integrity_guard();
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);

  let writable = volume.dax_map_write(&mut store, f).unwrap();
  let guarded = volume.dax_map_read(&mut store, f).unwrap();
  assert_eq!(store.dax.guards(), 1, "the read map is guarded");
  assert_eq!(
    guarded.block, writable.block,
    "both windows name the one content block, which is what makes the pin shared"
  );

  // The file goes; its block's free is deferred while anything still pins it.
  let root = volume.root_inode(&store).unwrap();
  volume.unlink_no(&mut store, root, "f").unwrap();

  // The windows are released. The shared per-block pin reaches zero here even though the read mapping is still
  // in the registry (§4.6's recorded map-storm defect), so the deferred free completes and scrubs the block.
  store.dax_revoke(&writable);
  store.dax_release_after_unmap(&writable);
  store.dax_release_after_unmap(&guarded);
  let freed = !store.content.arena().is_pinned(guarded.block);
  if !freed {
    println!(
      "SKIP the_net_never_heals_through_a_block_the_arena_has_freed: the block is still pinned here, so the \
       state this test is about did not arise"
    );
    return;
  }
  let after_free = store
    .content
    .arena()
    .bytes(guarded.block)
    .map(<[u8]>::to_vec)
    .unwrap_or_default();

  let slice = volume.dax_scrub_slice(&mut store, 0, u64::MAX);

  assert_eq!(
    slice.healed, 0,
    "the net wrote the deleted file's plaintext back into a freed block"
  );
  assert_eq!(
    slice.dropped, 1,
    "the stale guard was dropped instead (non-vacuity: the pass really reached it)"
  );
  assert_eq!(store.dax.guards(), 0, "and its clean copy went with it");
  assert_eq!(
    store
      .content
      .arena()
      .bytes(guarded.block)
      .map(<[u8]>::to_vec),
    Some(after_free),
    "the freed block's bytes are exactly as the free left them"
  );
}

/// §4.6 L1 (containment from the first window, not the first shift). The periodic shift arms within a liveness
/// budget of a volume's opt-in, so a window placed only by its first shift would spend that budget on
/// general-allocator rows, beside any tenant. §4.6 says the strategy "leans on *contain* (L1 zones) and *heal*
/// (B2) over *detect*", so containment is the layer that must not wait.
///
/// Do: map a file writable on a volume that has never shifted. Expect: the block is already a row of the
/// volume's own sandbox, with the guard rows untouched, and the content intact — no shift required. Where the
/// host backs no sandbox the map still succeeds with the block the allocator gave it, because the sandbox is
/// where the row *should* be and never a condition of serving the window; only the shift refuses, since a
/// shift is what would be theatre without it.
#[test]
fn a_writable_window_is_placed_in_its_sandbox_before_any_shift() {
  let mut store = store_sandboxed();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);

  let mapped = volume.dax_map_write(&mut store, f).unwrap();

  assert_eq!(
    store.dax.migrated(),
    0,
    "no shift has happened: placement is the map's own doing"
  );
  match store.dax_zones.of_volume(volume.prefix()) {
    Some(zone) => {
      let (_, slots) = zone.held();
      let slot = zone
        .slot_of(mapped.block)
        .expect("the first writable window is already on a row of the sandbox");
      assert_ne!(slot, 0, "and not on the leading guard row");
      assert_ne!(
        slot,
        slots.saturating_sub(1),
        "nor on the trailing guard row"
      );
      assert!(
        all_bytes(&store.dax_read_raw(&mapped, 0, 16), 0x41),
        "the content came with it"
      );
    }
    None => {
      println!(
        "SKIP a_writable_window_is_placed_in_its_sandbox_before_any_shift: this host backs no transparent \
         huge page, so §4.6's L1 sandbox cannot be reserved"
      );
      assert!(
        all_bytes(&store.dax_read_raw(&mapped, 0, 16), 0x41),
        "the window is served anyway, with its content"
      );
    }
  }
}

// ───────────────────────────────────────────────────────────── B2 the self-healing safety net (checksum + heal)

/// A synthetic Rowhammer bit flip: invert a byte of a block directly in the arena, behind the daemon's back — exactly
/// what a hardware hammer does to a mapped page that the daemon never sees written.
/// Writes `byte` at `at` in `block` — a corruption distinguishable from [`flip_a_byte`]'s, so a test can tell
/// which of two corrupted copies a heal actually used.
fn poison_a_byte(store: &mut Store, block: slates_mem::arena::Extent, at: usize, byte: u8) {
  if let Some(bytes) = store.content.arena_mut().bytes_mut(block)
    && let Some(b) = bytes.get_mut(at)
  {
    *b = byte;
  }
}

fn flip_a_byte(store: &mut Store, block: slates_mem::arena::Extent) {
  if let Some(bytes) = store.content.arena_mut().bytes_mut(block)
    && let Some(b) = bytes.get_mut(0)
  {
    *b ^= 0xFF;
  }
}

/// One whole scrub pass, driven in slices as the reap tick drives it: the blocks healed, the blocks found
/// clean, and the slices it took. The budget is one byte, so a slice stops after the first block it hashes and
/// the pass's slicing is what is under test.
fn run_one_scrub_pass(store: &mut Store, volume: &Volume) -> (u64, u64, usize) {
  let mut at = 0usize;
  let mut healed = 0u64;
  let mut clean = 0u64;
  let mut passes = 0usize;
  while at < store.dax.guards() {
    let slice = volume.dax_scrub_slice(store, at, 1);
    assert!(
      slice.next > at,
      "a slice always advances, so a pass terminates"
    );
    at = slice.next;
    healed = healed.saturating_add(slice.healed);
    clean = clean.saturating_add(slice.clean);
    passes = passes.saturating_add(1);
  }
  (healed, clean, passes)
}

/// §4.6 **B2's net, driven as the shard drives it** ([`Volume::dax_scrub_slice`]) — the pass the reap tick runs.
/// Do: opt a volume into the guard, map three files read-only so each is guarded at map time, flip a byte in
/// one behind the daemon's back as a hardware hammer would, then run a scrub pass in slices with a budget
/// smaller than the pass. Expect: every guard is visited, the flipped block is healed back to its clean bytes,
/// the others read clean, and the pass advances to the end. A flip in read-only content is invisible to slates
/// by construction — no write explains it — so this timer pass is the only thing that can find it.
#[test]
fn the_scrub_pass_heals_a_flip_no_write_explains_and_leaves_the_rest_clean() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  volume.enable_dax_integrity_guard();
  // Derived: three guards is the fewest that shows the pass slicing and still leaves clean neighbours either
  // side of the flipped one.
  const GUARDED: usize = 3;
  let mut mapped = Vec::new();
  for which in 0..GUARDED {
    let name = format!("f{which}");
    let f = file_of(&mut store, &mut volume, &name, 0x41, DAX_PAGE);
    mapped.push(volume.dax_map_read(&mut store, f).unwrap());
  }
  assert_eq!(
    store.dax.guards(),
    GUARDED,
    "the opt-in guarded every read-only map (non-vacuity: the net is armed)"
  );

  // A hardware hammer flips a byte in the middle one. Nothing in the daemon wrote it.
  let victim = mapped[1];
  flip_a_byte(&mut store, victim.block);
  assert!(
    !all_bytes(&store.dax_read_raw(&victim, 0, DAX_PAGE), 0x41),
    "the flip really changed the block the guest reads"
  );

  // The pass, in slices of one block each, as the reap tick runs it.
  let (healed, clean, passes) = run_one_scrub_pass(&mut store, &volume);
  assert_eq!(healed, 1, "the flipped block was healed");
  assert_eq!(
    clean,
    (GUARDED - 1) as u64,
    "the others were checked and found clean"
  );
  assert_eq!(
    passes, GUARDED,
    "the budget sliced the pass, one block each"
  );
  assert_eq!(store.dax.healed(), 1, "the registry counted the heal");
  assert!(
    all_bytes(&store.dax_read_raw(&victim, 0, DAX_PAGE), 0x41),
    "the guest reads its own bytes again — the flip was undone"
  );
}

/// §4.6 B2 (the opt-in is what arms the net). Do: map a file read-only on a volume that has *not* opted in, flip
/// a byte in it, and run the scrub pass. Expect: nothing was guarded, so the pass finds nothing to check and the
/// flip stands — which is the cost of not opting in, stated as a test rather than left implied. Non-vacuity: the
/// same volume, opted in and re-mapped, is guarded and heals.
#[test]
fn without_the_opt_in_nothing_is_guarded_and_the_scrub_pass_has_nothing_to_find() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let m = volume.dax_map_read(&mut store, f).unwrap();
  assert_eq!(store.dax.guards(), 0, "no opt-in, no guard");

  flip_a_byte(&mut store, m.block);
  let slice = volume.dax_scrub_slice(&mut store, 0, u64::MAX);
  assert_eq!(slice.clean, 0, "nothing was checked");
  assert_eq!(slice.healed, 0, "nothing was healed");
  assert_eq!(slice.next, 0, "the pass was over before it began");
  assert!(
    !all_bytes(&store.dax_read_raw(&m, 0, DAX_PAGE), 0x41),
    "the flip stands, which is what not opting in costs"
  );

  // Non-vacuity: the opt-in arms it, and the same flip heals.
  store.dax_revoke(&m);
  volume.enable_dax_integrity_guard();
  let armed = volume.dax_map_read(&mut store, f).unwrap();
  assert_eq!(store.dax.guards(), 1, "the opt-in guarded the re-map");
  flip_a_byte(&mut store, armed.block);
  let slice = volume.dax_scrub_slice(&mut store, 0, u64::MAX);
  assert_eq!(slice.healed, 1, "the armed net healed the same flip");
}

/// §4.6 multi-tenancy **B2** (the self-healing safety net). Do: guard a read-only mapping (immutable content), then
/// inject a synthetic bit flip into its block and scrub. Expect: the fresh guard scrubs clean; after the flip the
/// scrub catches it and heals the block from the clean copy, the victim byte erased; a re-scrub confirms it; the
/// flip and heal counters moved. This is the asynchronous net for the one hazard the daemon cannot see directly.
#[test]
fn a_flip_in_a_guarded_read_only_block_is_caught_and_healed() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let m = volume.dax_map_read(&mut store, f).unwrap();
  store.dax_guard(&m).unwrap();
  assert_eq!(
    store.dax_scrub(&m),
    DaxScrub::Clean,
    "a freshly guarded block scrubs clean"
  );

  flip_a_byte(&mut store, m.block);
  assert_eq!(
    store.dax_scrub(&m),
    DaxScrub::Healed,
    "the flip is caught and the block restored"
  );
  assert!(
    all_bytes(&store.dax_read_raw(&m, 0, 64), 0x41),
    "the victim byte is erased — the clean content is back"
  );
  assert_eq!(
    store.dax_scrub(&m),
    DaxScrub::Clean,
    "a re-scrub confirms the heal"
  );
  assert_eq!(store.dax.flips(), 1, "one flip caught (non-vacuity)");
  assert_eq!(store.dax.healed(), 1, "one block healed (non-vacuity)");
}

/// §4.6 B2 — adversarial: when the clean copy is corrupt too, the net must refuse, not "heal" with bad bytes.
/// Rowhammer flips **physical rows**, and the guard's clean copy lives in the same RAM as the block it protects,
/// so "both corrupt" is an ordinary state for this threat and not a contrived one. A defence that trusts its own
/// copy has an unguarded single point, and the failure would be the worst kind: a scrub reporting success while
/// writing an attacker's bytes over good content.
///
/// Do: guard a read-only mapping, flip a byte in the block **and** a byte in the clean copy, then scrub. Expect:
/// `Unhealable`; the block is left as it was rather than overwritten from the bad copy; the flip is counted and
/// no heal is.
///
/// This arm had no test at all — `DaxScrub::Unhealable` appeared only in the source — which is why it is written
/// here: a refusal nothing exercises is a refusal nobody knows still works.
#[test]
fn a_flip_in_the_clean_copy_too_is_refused_rather_than_healed_from_bad_bytes() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let m = volume.dax_map_read(&mut store, f).unwrap();
  store.dax_guard(&m).unwrap();
  let clean = store
    .dax
    .guard_clean_block(&m)
    .expect("the guard holds a clean copy");

  // The two corruptions must differ, or the test cannot see the failure it is for. Flipping the same byte the
  // same way in both would make the bad copy byte-identical to the bad block, so healing from it would change
  // nothing and "the block was not overwritten" would hold however the code behaved — a vacuous pass, and the
  // first version of this test had exactly that bug. So: the block loses byte 0, and the copy is poisoned at a
  // *different* offset, which a heal from it would stamp onto the block.
  flip_a_byte(&mut store, m.block);
  poison_a_byte(&mut store, clean, 1, POISON);
  let flipped_block = store.dax_read_raw(&m, 0, 64);
  let flips_before = store.dax.flips();
  let healed_before = store.dax.healed();

  assert_eq!(
    store.dax_scrub(&m),
    DaxScrub::Unhealable,
    "with both the block and its clean copy corrupt the scrub must refuse, not heal"
  );
  let after = store.dax_read_raw(&m, 0, 64);
  assert_eq!(
    after, flipped_block,
    "the block is left as it was, never overwritten from the corrupt copy"
  );
  assert!(
    after.get(1).is_some_and(|byte| *byte != POISON),
    "and the copy's poison never reached the block (it would have, had the heal run from it)"
  );
  assert_eq!(
    (
      store.dax.flips().saturating_sub(flips_before),
      store.dax.healed().saturating_sub(healed_before)
    ),
    (1, 0),
    "the flip is counted (non-vacuity: the scrub really looked) and no heal is claimed"
  );
}

/// §4.6 B2. Do: guard a read-only mapping and scrub it twice without any flip. Expect: both scrubs are clean, nothing
/// heals (no false positive on untouched immutable content), and the scrub counter moved.
#[test]
fn a_guarded_block_without_a_flip_scrubs_clean() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let m = volume.dax_map_read(&mut store, f).unwrap();
  store.dax_guard(&m).unwrap();

  assert_eq!(store.dax_scrub(&m), DaxScrub::Clean);
  assert_eq!(store.dax_scrub(&m), DaxScrub::Clean);
  assert_eq!(
    store.dax.flips(),
    0,
    "no false-positive flip on untouched content"
  );
  assert!(
    store.dax.scrubbed() >= 2,
    "the scrub counter moved (non-vacuity)"
  );
}

/// §4.6 B2 (the honest scope). Do: try to guard a *writable* mapping. Expect: refused `Invalid` — a writable block
/// changes legitimately through the guest's own page table, so the daemon cannot tell a flip from a write and a
/// content checksum would false-positive on every write; writable blocks are defended by the zones and the moving
/// target, not by B2. Non-vacuity: a read-only mapping of the same file guards.
#[test]
fn a_writable_mapping_cannot_be_guarded() {
  use slates_vfs::error::VfsError;
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);

  let w = volume.dax_map_write(&mut store, f).unwrap();
  assert!(
    matches!(store.dax_guard(&w), Err(VfsError::Invalid)),
    "a writable mapping cannot be content-guarded"
  );
  assert_eq!(
    store.dax.guards(),
    0,
    "no guard was registered for the writable mapping"
  );

  store.dax_revoke(&w);
  let r = volume.dax_map_read(&mut store, f).unwrap();
  assert!(
    store.dax_guard(&r).is_ok(),
    "a read-only mapping of the same file guards"
  );
  assert_eq!(store.dax.guards(), 1);
}

/// §4.6 B2. Do: guard a read-only mapping, then revoke it. Expect: the revoke drops the guard (and frees its clean
/// copy), so a guarded mapping never leaves a pinned clean block behind after teardown.
#[test]
fn revoking_a_guarded_mapping_drops_the_guard() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let m = volume.dax_map_read(&mut store, f).unwrap();
  store.dax_guard(&m).unwrap();
  assert_eq!(store.dax.guards(), 1);

  assert!(store.dax_revoke(&m), "the mapping is revoked");
  assert_eq!(
    store.dax.guards(),
    0,
    "the revoke dropped the guard and freed its clean copy"
  );
}

/// §4.6 B2 + the step-5 two-phase revoke. A write or truncate of a DAX-mapped inode revokes *every* mapping of it —
/// including a read-only guarded one — through the two-phase path (`dax_revoke_pending`, pin kept until the device
/// confirms the unmap), not the synchronous `dax_revoke`. The guard's clean copy is a separate pinned block that the
/// phase-one revoke must free, or it leaks (the mapped block it protected is gone). Do: guard a read-only mapping,
/// revoke it pending, then release. Expect: phase one drops the guard and frees its clean copy; the mapped block's
/// pin is kept until phase two; and far more guard/revoke cycles than the arena holds never exhaust it (a leaked
/// clean copy per cycle would). Before the fix, phase one only unregistered the mapping and left the guard pinned.
#[test]
fn the_two_phase_revoke_drops_a_guarded_mappings_clean_copy() {
  let mut store = store_capped_chunks(16);
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let m = volume.dax_map_read(&mut store, f).unwrap();
  let block = m.block;
  store.dax_guard(&m).unwrap();
  assert_eq!(store.dax.guards(), 1, "the read-only mapping is guarded");

  assert!(
    store.dax_revoke_pending(&m),
    "phase one: the mapping was held"
  );
  assert_eq!(
    store.dax.guards(),
    0,
    "phase one dropped the guard and freed its clean copy — no pinned clean block is left behind"
  );
  assert!(
    store.content.arena().is_pinned(block),
    "phase one still keeps the MAPPED block pinned (the stale-TLB hold); only the guard's clean copy was freed"
  );
  assert!(
    store.dax_release_after_unmap(&m),
    "phase two releases the mapped block's pin"
  );

  // No leak across cycles: the 16-chunk arena would be exhausted by a leaked clean copy (guard + mapped block + the
  // clean copy are three blocks per cycle), but a freed clean copy lets every cycle reuse the space.
  for cycle in 0..64 {
    let m = volume
      .dax_map_read(&mut store, f)
      .unwrap_or_else(|e| panic!("map after {cycle} guarded two-phase revokes: {e:?} — a leaked clean copy exhausted the arena"));
    store.dax_guard(&m).unwrap_or_else(|e| {
      panic!("guard on cycle {cycle}: {e:?} — a leaked clean copy exhausted the arena")
    });
    assert!(store.dax_revoke_pending(&m), "phase one, cycle {cycle}");
    assert_eq!(store.dax.guards(), 0, "guard dropped on cycle {cycle}");
    assert!(
      store.dax_release_after_unmap(&m),
      "phase two, cycle {cycle}"
    );
  }
}

/// §4.6 step 5 + CLAUDE.md #8 (the daemon→device pending-revoke queue is bounded, deduped, and non-vacuous — a
/// silently-dead enqueue cannot pass for a working channel). A write or truncate of a mapped inode enqueues it for
/// the device to munmap; the queue must (a) enqueue a mapped inode exactly once however many times it is written
/// before the device drains (a write storm cannot grow it per write), and (b) never exceed the registry cap, since
/// only a *mapped* inode enqueues and live mappings are themselves cap-bounded. Do: write one mapped inode twice
/// (re-mapping between, as a guest would) without a drain, then several distinct mapped inodes once each; drain and
/// count. Expect: the storm leaves one entry; the distinct inodes leave one each; the drain clears the queue.
#[test]
fn the_pending_revoke_queue_is_bounded_and_deduped() {
  use std::collections::BTreeSet;
  const CAP: usize = 16;
  let mut store = store_capped_chunks(CAP);
  let mut volume = volume(&mut store, 1 << 20);

  // A write storm on one mapped inode: each write revokes-and-enqueues, but the inode is deduped to one entry. The
  // guest re-maps after each write (its window was torn), exactly as the device path makes it; the queue still holds
  // the inode once until a drain.
  let f = file_of(&mut store, &mut volume, "f", 0x11, DAX_PAGE);
  for round in 0..5u8 {
    let m = volume.dax_map_read(&mut store, f).unwrap();
    assert!(store.dax_is_live(&m), "the re-map is live, round {round}");
    volume.write(&mut store, f, 0, &[round; 8]).unwrap();
    assert!(
      store.dax.pending_len() <= 1,
      "the storm never grows the queue past the one deduped inode"
    );
  }
  assert_eq!(
    store.dax.take_pending_revokes(),
    vec![(f, false)],
    "a write storm on one mapped inode enqueues it once, writable_only=false (every window stale)"
  );
  assert!(
    store.dax.take_pending_revokes().is_empty(),
    "the drain cleared the queue"
  );

  // Several distinct mapped inodes, each written once: each enqueued exactly once, and the queue stays within the cap.
  let mut want: BTreeSet<u64> = BTreeSet::new();
  for name in ["a", "b", "c", "d"] {
    let no = file_of(&mut store, &mut volume, name, 0x22, DAX_PAGE);
    let m = volume.dax_map_read(&mut store, no).unwrap();
    assert!(store.dax_is_live(&m));
    volume.write(&mut store, no, 0, &[0xCD; 8]).unwrap();
    want.insert(no.0);
  }
  assert!(
    store.dax.pending_len() <= CAP,
    "the queue never exceeds the registry cap"
  );
  let drained: BTreeSet<u64> = store
    .dax
    .take_pending_revokes()
    .into_iter()
    .map(|(no, _)| no.0)
    .collect();
  assert_eq!(
    drained, want,
    "each distinct mapped inode was enqueued exactly once"
  );
}

/// §4.6 step 5 — snapshot/clone now revokes its volume's writable DAX windows **two-phase** (the device munmaps),
/// and a retrying `snapshot` watches the registry to know when it is safe to freeze. Do: map one file writable and
/// another read-only in a volume, then `dax_revoke_writable_of_prefix`. Expect: the writable mapping leaves the
/// registry revoke-pending (its block kept pinned) and is enqueued `writable_only=true` (the device tears only the
/// writable window); the read-only mapping stays live; `has_writable_of_prefix` goes false (snapshot's "windows
/// handed to the device") and `has_pending_of_prefix` goes true (snapshot retries until the device drains it); and
/// the two-phase release then unpins. This is the VFS half of the writable-DAX↔snapshot recall; the device half is
/// `request_revoke_writable_inode`, and the retry is in the `snapshot` verb.
#[test]
fn a_snapshot_revokes_the_volumes_writable_windows_two_phase_and_keeps_read_only() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let prefix = volume.prefix();
  let w = file_of(&mut store, &mut volume, "w", 0x11, DAX_PAGE);
  let r = file_of(&mut store, &mut volume, "r", 0x22, DAX_PAGE);
  let wm = volume.dax_map_write(&mut store, w).unwrap();
  let rm = volume.dax_map_read(&mut store, r).unwrap();

  // The snapshot's teardown: writable windows go revoke-pending + enqueued; the read-only one survives.
  assert_eq!(
    store.dax_revoke_writable_of_prefix(prefix),
    1,
    "exactly the writable mapping was revoked-pending"
  );
  let state_after = (
    store.dax.has_writable_of_prefix(prefix),
    store.dax.has_pending_of_prefix(prefix),
    store.dax_is_live(&rm),
    store.content.arena().is_pinned(wm.block),
  );
  assert_eq!(
    state_after,
    (false, true, true, true),
    "writable handed to the device (not live), pending for snapshot to await, read-only survives, block still \
     pinned (phase-one stale-TLB hold)"
  );

  // The device drains (modelled: take the queue writable_only=true, then release-after-unmap).
  assert_eq!(
    store.dax.take_pending_revokes(),
    vec![(wm.inode, true)],
    "the writable revoke is enqueued writable_only=true (device keeps the read-only window)"
  );
  let drained_state = (
    store.dax.has_pending_of_prefix(prefix),
    store.dax_release_after_unmap(&wm),
  );
  assert_eq!(
    drained_state,
    (false, true),
    "the take cleared the queue (snapshot may freeze) and phase two unpins"
  );
}

/// §4.6 step 5 — adversarial: a guest must not be able to hold a snapshot off forever by re-arming the window the
/// snapshot just called back. Do: map a file writable, run the snapshot's recall
/// (`dax_revoke_writable_of_prefix`), and then do what a hostile guest does — ask for a writable map again, of that
/// file and of a *second* file of the same volume. Expect: both refused `RecallInFlight`, so the snapshot's next
/// retry finds no new writable window and converges; once the recall ends, a writable map is admitted again.
///
/// Non-vacuity, both ways: the same map succeeds before the recall and again after it ends, so the refusal is the
/// recall's and not a map that never worked; and a *read* map is admitted throughout, so the exclusion is of
/// writable windows only and not of DAX as such.
#[test]
fn a_guest_cannot_re_arm_a_writable_window_while_the_snapshot_is_recalling_it() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let prefix = volume.prefix();
  let first = file_of(&mut store, &mut volume, "first", 0x11, DAX_PAGE);
  let second = file_of(&mut store, &mut volume, "second", 0x22, DAX_PAGE);

  // Before: a writable map is admitted. (Non-vacuity for the refusals below.)
  let mapped = volume.dax_map_write(&mut store, first).unwrap();
  assert!(
    store.dax_is_live(&mapped),
    "the window is live before the recall"
  );

  // The snapshot's recall.
  assert_eq!(
    store.dax_revoke_writable_of_prefix(prefix),
    1,
    "the live writable window was handed to the device"
  );
  // What the `snapshot` verb does with the revoke: the exclusion is the retry loop's, so the caller that retries
  // begins it and ends it (`crates/server/src/verbs.rs`). The revoke itself deliberately does not.
  assert!(store.dax.begin_recall(prefix), "the recall is admitted");
  assert!(store.dax.recalling(prefix), "the volume is recalling");

  // The attack: re-arm a writable window while the snapshot waits for the device.
  let retaken = volume.dax_map_write(&mut store, first);
  let neighbour = volume.dax_map_write(&mut store, second);
  assert_eq!(
    (retaken.err(), neighbour.err()),
    (
      Some(slates_vfs::error::VfsError::RecallInFlight),
      Some(slates_vfs::error::VfsError::RecallInFlight)
    ),
    "neither the recalled file nor a neighbour of the same volume can be re-armed during the recall"
  );
  assert!(
    !store.dax.has_writable_of_prefix(prefix),
    "so the snapshot's retry still sees no live writable window — the retry converges"
  );

  // A read map is untouched: the exclusion is of writable windows, not of DAX.
  let read_through = volume.dax_map_read(&mut store, second).unwrap();
  assert!(
    store.dax_is_live(&read_through),
    "read-only DAX is admitted during a recall"
  );

  // The snapshot finishes and ends its recall; writable maps are admitted again.
  store.dax.end_recall(prefix);
  let after = volume.dax_map_write(&mut store, first);
  assert!(
    after.is_ok(),
    "once the recall ends a writable window is admitted again: {after:?}"
  );
}

/// §4.6 step 5 — the recall's lifetime has no loose ends. Do: begin recalls for several volumes, re-begin one of
/// them, destroy one, and push past the registry's cap. Expect: the list is deduped (re-beginning changes nothing),
/// a destroyed volume's recall goes with it (so a later volume reusing the prefix is not blocked by a ghost), and
/// the list never grows past the cap — it is bounded like every other structure here (CLAUDE.md #8).
#[test]
fn recalls_are_deduped_bounded_and_released_by_destroy() {
  let mut store = store();
  let volume = volume(&mut store, 1 << 20);
  let prefix = volume.prefix();

  assert!(
    store.dax.begin_recall(prefix),
    "the first recall is admitted"
  );
  assert!(store.dax.begin_recall(prefix), "re-beginning is idempotent");
  assert_eq!(store.dax.recalling_len(), 1, "one volume, one entry");

  // A destroy takes the recall with it.
  store.dax_revoke_volume(prefix);
  assert!(
    !store.dax.recalling(prefix),
    "a destroyed volume leaves no recall behind for its prefix's next holder"
  );
  assert_eq!(store.dax.recalling_len(), 0, "and nothing in the list");

  // The bound, on a registry of its own with a small cap: a live store's cap is larger than the whole prefix
  // space, so filling it there would measure nothing.
  let cap = 3usize;
  let mut small = slates_vfs::dax::DaxRegistry::new(cap);
  let mut admitted = 0usize;
  for candidate in 1..=u16::try_from(cap.saturating_add(5)).unwrap_or(u16::MAX) {
    if small.begin_recall(candidate) {
      admitted = admitted.saturating_add(1);
    }
  }
  assert_eq!(
    (admitted, small.recalling_len()),
    (cap, cap),
    "the list stops at the cap and refuses the rest, instead of growing with the caller"
  );
}

/// §4.6 (the DAX device-window backing seam, Unix). Do: on the scratch (anonymous-backed) test store, map a file
/// writable and ask for its DAX backing; then revoke and ask again. Expect: `None` both times — a scratch volume's
/// content is private process RAM with no descriptor to hand a VMM (so DAX is served only on anchor-backed volumes),
/// and a revoked mapping has nothing to map. This confines the DAX device window to anchor-backed volumes by
/// construction, not by a check the device could skip.
#[cfg(unix)]
#[test]
fn a_scratch_volume_has_no_dax_backing_and_a_revoked_mapping_has_none() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let m = volume.dax_map_write(&mut store, f).unwrap();
  assert!(
    store.dax_backing(&m).is_none(),
    "a scratch (anonymous-backed) volume has no DAX backing fd to hand a VMM"
  );
  store.dax_revoke(&m);
  assert!(
    store.dax_backing(&m).is_none(),
    "a revoked mapping has no DAX backing"
  );
}

/// Parses the flat READDIRPLUS directory snapshot `dax_map_dir` writes (§4.6 metadata DAX): a u32 count, then per
/// entry a u64 inode, a kind byte, a u64 size, an i64 mtime, u32 mode/uid/gid/nlink, a name length, and the name.
/// Returns name → (inode, size, mode), so the test checks each entry's attributes came through, not just its name.
fn parse_dir_snapshot(raw: &[u8]) -> std::collections::BTreeMap<String, (u64, u64, u32)> {
  let mut out = std::collections::BTreeMap::new();
  let count = u32::from_le_bytes(raw[0..4].try_into().unwrap());
  let mut off = 4usize;
  for _ in 0..count {
    let inode = u64::from_le_bytes(raw[off..off + 8].try_into().unwrap());
    let size = u64::from_le_bytes(raw[off + 9..off + 17].try_into().unwrap());
    let mode = u32::from_le_bytes(raw[off + 25..off + 29].try_into().unwrap());
    let name_len = usize::from(raw[off + 41]);
    let name = String::from_utf8(raw[off + 42..off + 42 + name_len].to_vec()).unwrap();
    out.insert(name, (inode, size, mode));
    off += 42 + name_len;
  }
  out
}

/// §4.6 metadata DAX (READDIRPLUS). Do: create three named files in a directory, map the directory with
/// `dax_map_dir`, and read the window the way a guest would (through its own page table). Expect: the mapped block
/// is the flat snapshot — a count then a record per entry carrying the inode, kind, and the entry's attributes
/// (size, mode, …) and name — matching the directory's `readdir` plus each entry's `stat`, so a guest lists the
/// directory AND reads each entry's attributes with no FUSE READDIR or GETATTR round-trip. Then revoke and confirm
/// the mapping is dead and its throwaway block is **freed, not leaked**: mapping and revoking far more times than the
/// (chunk-capped) content arena holds would exhaust it on a leak, but a freed-and-reused block never does.
#[test]
fn a_directory_maps_its_entries_flat_and_revoke_frees_the_snapshot_block() {
  let mut store = store_capped_chunks(16);
  let mut volume = volume(&mut store, 1 << 20);
  let root = volume.root_inode(&store).unwrap();
  let mut want: std::collections::BTreeMap<String, (u64, u64, u32)> =
    std::collections::BTreeMap::new();
  for name in ["alpha", "beta", "gamma"] {
    let no = volume
      .create_file_no(&mut store, root, name, 0o644)
      .unwrap();
    let a = volume.stat(&store, no).unwrap();
    want.insert(name.to_owned(), (no.0, a.size, a.mode));
  }

  let mapping = volume.dax_map_dir(&mut store, root).unwrap();
  assert!(
    mapping.snapshot,
    "a directory mapping owns its throwaway block"
  );
  assert!(store.dax_is_live(&mapping), "the directory mapping is live");
  let raw = store.dax_read_raw(&mapping, 0, mapping.file_len);
  assert_eq!(
    parse_dir_snapshot(&raw),
    want,
    "the window is the directory's entries, flat and guest-readable, no READDIR round-trip"
  );
  assert!(store.dax_revoke(&mapping), "the directory mapping revokes");
  assert!(
    !store.dax_is_live(&mapping),
    "the mapping is dead after revoke"
  );

  // No leak: far more map/revoke cycles than the 16-chunk arena holds. A leaked snapshot block would exhaust it.
  for cycle in 0..64 {
    let m = volume.dax_map_dir(&mut store, root).unwrap_or_else(|e| {
      panic!("map after {cycle} revokes: {e:?} — a leaked snapshot block exhausted the arena")
    });
    assert!(store.dax_revoke(&m), "revoke cycle {cycle}");
  }
}

/// A trivial in-test chunk cipher (A-99 shape): seals by XOR-ing every byte (so a sealed block's arena bytes differ
/// from the plaintext — a guest MMU reading it raw would see ciphertext) and opens by XOR-ing back under a fixed tag.
/// Enough to make the idle sweep actually *encrypt*, which is what the pin-defers-seal guard must prevent under a live
/// window.
struct XorSealCipher;
impl slates_vfs::content::ChunkCipher for XorSealCipher {
  fn seal(
    &self,
    _key: u32,
    _version: u64,
    _index: u32,
    _last: bool,
    segment: &mut [u8],
  ) -> Result<slates_vfs::content::Tag, VfsError> {
    for b in segment.iter_mut() {
      *b ^= 0xFF;
    }
    Ok([0xAB; 16])
  }
  fn open(
    &self,
    _key: u32,
    _version: u64,
    _index: u32,
    _last: bool,
    segment: &mut [u8],
    tag: &slates_vfs::content::Tag,
  ) -> Result<(), VfsError> {
    if *tag != [0xAB; 16] {
      return Err(VfsError::Integrity);
    }
    for b in segment.iter_mut() {
      *b ^= 0xFF;
    }
    Ok(())
  }
  fn identity(&self, _key: u32) -> Option<slates_vfs::content::KeyIdentity> {
    Some([0u8; 32])
  }
  fn reference(&mut self, _identity: &slates_vfs::content::KeyIdentity) -> Result<u32, VfsError> {
    Ok(1)
  }
  fn key_for_volume(&mut self, _volume: [u8; 16]) -> Result<u32, VfsError> {
    Ok(1)
  }
}

/// §4.6 (A-135 / the open-frontier rule, **pin-defers-seal**). Do: with at-rest sealing enabled (a key + a cipher),
/// map a single-block file read-only, then run the idle sweep that would seal — AES-GCM-encrypt in place (A-92/A-99)
/// — its open block. Expect: the sweep **defers** the seal while a live window pins the block (sealing it would swap
/// plaintext→ciphertext under the guest's MMU), so the mapping stays current and reads its plaintext; once the mapping
/// is revoked and the block unpins, a later sweep seals it. Before the guard the sweep sealed the mapped block, so the
/// mapping went stale (`dax_still_current` false) and a hardware read saw ciphertext.
#[test]
fn the_idle_sweep_defers_sealing_a_mapped_block_until_it_is_unmapped() {
  let mut store = store();
  // Turn on at-rest sealing: a cipher on the store and a key on the volume, so the sweep actually encrypts.
  store.content.set_cipher(Box::new(XorSealCipher));
  let mut volume = volume(&mut store, 1 << 20);
  volume.set_seal_key(Some(1));
  let f = file_of(&mut store, &mut volume, "f", 0x5A, DAX_PAGE);

  let mapping = volume.dax_map_read(&mut store, f).unwrap();
  assert!(
    volume.dax_still_current(&store, &mapping),
    "mapped and current before the sweep"
  );

  // Two ticks age f into the pending set; the sweep then tries to seal it, but the live window pins its block.
  volume.age_writes();
  volume.age_writes();
  let _ = volume.seal_pending(&mut store, u64::MAX);
  assert!(
    volume.dax_still_current(&store, &mapping),
    "the mapped block was NOT sealed under the live window (the sweep deferred it)"
  );
  let page = volume.dax_read(&store, &mapping, 0, DAX_PAGE).unwrap();
  assert!(
    page.iter().all(|&b| b == 0x5A),
    "the window still reads the file's plaintext, not XOR ciphertext: {:?}",
    &page[..8]
  );

  // Revoke the mapping (the no-live-guest synchronous path unpins immediately); now the sweep may seal it.
  assert!(
    store.dax_revoke(&mapping),
    "the mapping is revoked and the block unpinned"
  );
  volume.age_writes();
  volume.age_writes();
  let _ = volume.seal_pending(&mut store, u64::MAX);
  assert!(
    !volume.dax_still_current(&store, &mapping),
    "once unmapped, the sweep sealed the block (the deferral lifted)"
  );
}

/// A multi-block file sealed at rest (a cipher + a key): one full head chunk [0, chunk) and an open tail
/// [chunk, chunk + page), written `0x33`. The two reopen-arm tests below both start here.
fn sealed_head_open_tail(store: &mut Store, volume: &mut Volume) -> (InodeNo, usize, usize) {
  let chunk = store.content.chunk_bytes();
  let root = volume.root_inode(store).unwrap();
  let no = volume.create_file_no(store, root, "big", 0o644).unwrap();
  volume.set_seal_key(Some(1));
  let tail = DAX_PAGE;
  volume
    .write(store, no, 0, &vec![0x33u8; chunk + tail])
    .unwrap();
  (no, chunk, tail)
}

/// §4.6 REOPEN arm (A/B vs view). Do: on a multi-block file (a sealed head + an open tail), map-read at an offset in
/// the OPEN TAIL and at offset 0 (the SEALED head). Expect: the open tail maps directly (zero-copy, a non-snapshot
/// mapping), and the sealed head is REOPENED IN PLACE — decrypted in its OWN canonical block (`snapshot: false`, one
/// representation, no throwaway) and mapped directly; both read their plaintext (the rule-(a) SIGBUS is gone).
#[test]
fn the_open_tail_maps_directly_and_the_sealed_head_is_reopened_in_place() {
  let mut store = store();
  store.content.set_cipher(Box::new(XorSealCipher));
  let mut volume = volume(&mut store, 1 << 24);
  let (no, chunk, tail) = sealed_head_open_tail(&mut store, &mut volume);

  // The open tail maps directly at its file offset (zero-copy, a non-snapshot mapping).
  let tail_map = volume.dax_map_read_at(&mut store, no, chunk).unwrap();
  assert_eq!(
    tail_map.block_file_start, chunk,
    "the tail mapping begins at the open tail's offset"
  );
  assert_eq!(
    tail_map.file_len, tail,
    "the tail mapping's length is the open tail's bytes"
  );
  assert!(
    !tail_map.snapshot,
    "the open tail is a direct (non-snapshot) mapping"
  );
  let got = volume.dax_read(&store, &tail_map, 0, tail).unwrap();
  assert!(all_bytes(&got, 0x33), "the open tail reads its plaintext");

  // Offset 0 is in the sealed head -> the REOPEN arm decrypts the chunk IN PLACE and maps its own block directly (no
  // throwaway snapshot — one representation), reading the chunk's plaintext.
  let head_map = volume.dax_map_read_at(&mut store, no, 0).unwrap();
  assert_eq!(
    head_map.block_file_start, 0,
    "the head mapping begins at offset 0"
  );
  assert_eq!(
    head_map.file_len, chunk,
    "the head mapping spans the sealed chunk's bytes"
  );
  assert!(
    !head_map.snapshot,
    "the reopened head is a direct (non-snapshot) mapping — one representation, no copy"
  );
  let head_bytes = volume.dax_read(&store, &head_map, 0, DAX_PAGE).unwrap();
  assert!(
    all_bytes(&head_bytes, 0x33),
    "the reopened head reads its plaintext"
  );
}

/// §4.6 the LEARNED re-seal hold. Do: map a sealed chunk (reopening it), revoke it and sweep — which re-seals it at
/// the starting hold of zero — then map it again straight away, so the guest came back inside the hold. Expect: the
/// gate counts that re-seal as premature and the hold grows by one sweep; and with the hold learned, a later revoke
/// plus one sweep leaves the chunk reopened (plaintext, ready for the guest's return) instead of re-encrypting it,
/// while a further sweep past the hold does re-seal it. Measured against a real guest kernel the revoke→remap
/// interval moved 8x with the guest's cache size, which is why this is learned rather than fixed.
#[test]
fn the_reseal_hold_is_learned_from_a_guest_that_comes_back() {
  let mut store = store();
  store.content.set_cipher(Box::new(XorSealCipher));
  let mut volume = volume(&mut store, 1 << 24);
  let (no, _chunk, _tail) = sealed_head_open_tail(&mut store, &mut volume);
  assert_eq!(volume.reseal_hold(), (0, 0), "nothing learned yet");

  // At hold zero the sweep re-seals as soon as the window is gone.
  let first = volume.dax_map_read_at(&mut store, no, 0).unwrap();
  assert!(store.dax_revoke(&first), "the first window is revoked");
  volume.seal_idle(&mut store, u64::MAX);
  assert!(
    !volume.dax_still_current(&store, &first),
    "re-sealed at hold zero"
  );

  // The guest comes straight back: that encrypt and this decrypt were both avoidable, and the volume learns.
  let second = volume.dax_map_read_at(&mut store, no, 0).unwrap();
  assert_eq!(
    volume.reseal_hold(),
    (1, 1),
    "the premature re-seal is counted and the hold grew a sweep"
  );

  // With the hold learned, one sweep after the revoke no longer re-encrypts.
  assert!(store.dax_revoke(&second), "the second window is revoked");
  volume.seal_idle(&mut store, u64::MAX);
  assert!(
    volume.dax_still_current(&store, &second),
    "held plaintext through one sweep for the guest's return"
  );
  // Held, not abandoned: the sweep past the hold re-seals it.
  volume.seal_idle(&mut store, u64::MAX);
  assert!(
    !volume.dax_still_current(&store, &second),
    "re-sealed once the hold passed"
  );
}

/// §4.6 REOPEN arm (A/B vs view), the cold transition. Do: reopen a sealed head chunk for a DAX map, then revoke the
/// map and run the idle sweep. Expect: while mapped the reopened chunk is current; once the map is revoked (the block
/// unpins) the sweep RE-SEALS it (ciphertext at rest again), so the stale mapping no longer validates — the guest
/// would re-map and re-reopen it. (Content is plaintext at rest only while a guest maps it.)
#[test]
fn a_reopened_dax_chunk_is_resealed_once_its_window_is_revoked() {
  let mut store = store();
  store.content.set_cipher(Box::new(XorSealCipher));
  let mut volume = volume(&mut store, 1 << 24);
  let (no, _chunk, _tail) = sealed_head_open_tail(&mut store, &mut volume);

  let head_map = volume.dax_map_read_at(&mut store, no, 0).unwrap();
  assert!(
    volume.dax_still_current(&store, &head_map),
    "the reopened head is current while mapped"
  );
  assert!(
    store.dax_revoke(&head_map),
    "the head mapping is revoked and its block unpinned"
  );
  volume.seal_idle(&mut store, u64::MAX);
  assert!(
    !volume.dax_still_current(&store, &head_map),
    "once unmapped, the sweep re-sealed the reopened head chunk"
  );
}

// ───────────────────────────────────────────────────────────── chaos: the gate under random interleavings

/// How many files the chaos schedule plays over: enough that a mapping of one can be caught by another's write,
/// truncate or seal, few enough that every step touches a file some mapping already holds.
const CHAOS_FILES: usize = 3;

/// One step of the chaos schedule, applied to the volume. Every operation is allowed to fail — a map can be
/// refused (no space, a sealed chunk, a multi-block file), a write can hit quota — and a refusal is itself a
/// valid outcome; what the invariants check is what the gate shows *afterwards*, never that a step succeeded.
fn chaos_step(
  store: &mut Store,
  volume: &mut Volume,
  files: &[InodeNo],
  held: &mut Vec<Mapping>,
  (op, which, size): (u8, u8, u16),
) -> Result<(), TestCaseError> {
  let no = files[usize::from(which) % files.len()];
  let len = usize::from(size) % (DAX_PAGE * 2);
  match op % 10 {
    0 => {
      let _ = volume.write(store, no, 0, &vec![op ^ which; len.max(1)]);
    }
    1 => {
      let _ = volume.truncate(store, no, len as u64);
    }
    2 => {
      if let Ok(mapping) = volume.dax_map_read(store, no) {
        held.push(mapping);
      }
    }
    3 => {
      // The exclusion asserted where it is exercised: while the volume is recalling its writable windows, a
      // writable map must be refused `RecallInFlight` — that refusal is what makes a snapshot's retry converge
      // instead of a guest re-arming the window between two of them. Checked before the call so the schedule
      // itself proves the rule under interleavings no hand-written case covers.
      let recalling = store.dax.recalling(volume.prefix());
      match volume.dax_map_write(store, no) {
        Ok(mapping) => {
          prop_assert!(
            !recalling,
            "a writable window was armed while the volume was recalling: the snapshot's retry can never converge"
          );
          held.push(mapping);
        }
        Err(e) => prop_assert!(
          !recalling || e == VfsError::RecallInFlight,
          "a recalling volume refused a writable map for the wrong reason: {e:?}"
        ),
      }
    }
    4 => {
      // One unpin per window. `dax_revoke` is the synchronous teardown: it unregisters *and* unpins, which is
      // what a client's own map release does (`reap::drop_client`). Calling `dax_release_after_unmap` after it
      // — phase two of the *two-phase* revoke, whose phase one is `dax_revoke_pending` and keeps the pin — is a
      // second unpin, and the arena's holder count cannot tell whose pin it takes: with two windows over one
      // block it steals the other's. This harness did exactly that until 2026-10-19, which is the only way the
      // "live mapping over an unpinned block" states the layered chaos property reported were reachable.
      if let Some(mapping) = held.pop() {
        store.dax_revoke(&mapping);
      }
    }
    5 => {
      // A snapshot splits shared content copy-on-write, which must revoke a writable mapping of it.
      let _ = volume.snapshot(store);
    }
    6 => {
      // Metadata DAX: a directory's entries materialized into a throwaway block the mapping owns, whose
      // release (`free_snapshot_block`) is a lifecycle a file's content block does not have.
      if let Ok(root) = volume.root_inode(&*store)
        && let Ok(mapping) = volume.dax_map_dir(store, root)
      {
        held.push(mapping);
      }
    }
    7 => {
      // What the `snapshot` verb does before it freezes: hand the volume's writable windows to the device and
      // put the recall on, so no new writable window can be armed while it waits (§4.6 step 5).
      let prefix = volume.prefix();
      store.dax_revoke_writable_of_prefix(prefix);
      let _ = store.dax.begin_recall(prefix);
    }
    8 => {
      // And what its wrapper does on any answer but the retryable one: the recall is this snapshot's to end.
      store.dax.end_recall(volume.prefix());
    }
    _ => {
      volume.age_writes();
      let _ = volume.seal_pending(store, 1 << 20);
    }
  }
  Ok(())
}

/// The invariants that must hold after every step, for every mapping the schedule still holds.
fn chaos_invariants(store: &Store, volume: &Volume, held: &[Mapping]) -> Result<(), TestCaseError> {
  // The recall's own bound, whatever the schedule did: one entry per volume, never past the registry's cap.
  prop_assert!(
    store.dax.recalling_len() <= store.dax.cap(),
    "the recall list grew past the registry's cap"
  );
  for mapping in held {
    if store.dax_is_live(mapping) && volume.dax_still_current(store, mapping) {
      let window = volume.dax_read(store, mapping, 0, mapping.page_span());
      prop_assert!(
        window.is_ok(),
        "a live, current mapping must read: {window:?}"
      );
      let bytes = window.unwrap_or_default();
      prop_assert!(
        bytes.len() >= mapping.file_len,
        "the window covers at least the file"
      );
      prop_assert!(
        bytes[mapping.file_len..].iter().all(|byte| *byte == 0),
        "a live mapping shows zeroes past its file, never another file's bytes or a stale tail"
      );
      // The stronger half: the window is this file's CURRENT bytes. Zeroes past the end would still pass
      // above if the head were stale — a copy-on-write split the mapping outlived, say. Skipped for a
      // materialized snapshot (a directory's entries): its block is a throwaway the mapping owns, there is no
      // file to read back, and a directory window is point-in-time by design — a mutation revokes it, it does
      // not track. Those mappings are still held to the live/inert invariants above and below.
      if !mapping.snapshot {
        let mut truth = vec![0u8; mapping.file_len];
        prop_assert!(
          volume.read(store, mapping.inode, 0, &mut truth).is_ok(),
          "the mapped file reads"
        );
        prop_assert_eq!(
          bytes.get(..mapping.file_len),
          Some(truth.as_slice()),
          "a live mapping shows the file's current bytes"
        );
      }
    } else {
      prop_assert!(
        volume.dax_read(store, mapping, 0, DAX_PAGE).is_err(),
        "a revoked or stale mapping is inert — the daemon path refuses it"
      );
    }
  }
  Ok(())
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(if cfg!(miri) { 4 } else { 150 }), .. ProptestConfig::default() }))]

  /// §4.6 / AC-4.12 under chaos. Every other DAX test here is one hand-built attack; this is the same gate under
  /// random interleavings of the operations that fight each other — writes and truncates that must revoke a
  /// mapping, maps read and writable, explicit unmaps, and the idle sweep that seals a block out from under a
  /// map. Do: play a random schedule over three files, holding every mapping the schedule makes. Expect, after
  /// **every** step: a mapping that is live and current reads, and shows zeroes past its file's length — never a
  /// stale tail and never another file's bytes; a mapping that is revoked or stale is inert, refused by the
  /// daemon path exactly as the MMU would make it inert on the hardware path. Any step may be refused — a
  /// refusal is a valid outcome — so what is asserted is what the gate SHOWS, not that an operation succeeded.
  ///
  /// This is the DAX gate's first property coverage: the 48 tests above are hand-written cases, and the
  /// interleavings that matter here are exactly the ones nobody writes by hand.
  ///
  /// Red-checking it established something worth keeping: **the eager revoke and the staleness check are
  /// independently sufficient**. Disabling `store.dax_revoke_inode` on write and truncate leaves the schedule
  /// green, because `dax_still_current` then catches the moved block and the mapping takes the inert branch;
  /// disabling `dax_still_current` alone leaves it green too, because the revoke already removed the mapping
  /// from the registry. Only with **both** disabled does a live mapping show a stale head and the
  /// current-bytes assertion fire. So a single red-check here proves nothing about this assertion — that is
  /// the defence being doubly redundant, not the test being vacuous (4,110 non-trivial comparisons over 150
  /// cases).
  #[test]
  fn a_random_interleaving_never_shows_a_live_mapping_stale_or_foreign_bytes(
    // Interpreted, this schedule had no coverage at all: one generated case of up to 59 steps did not finish
    // inside 1,800 s under Miri, so the Miri leg skipped it by name and the property went unchecked for
    // undefined behaviour. Shorter schedules, fewer of them, are the honest alternative to skipping -- a
    // handful of steps still interleaves the operations that fight each other (a write or truncate that must
    // revoke, a map held across it, an explicit unmap, the sealing sweep), which is the shape Miri reads.
    // Native runs keep the full 150 cases of up to 59 steps.
    schedule in prop::collection::vec(
      (any::<u8>(), any::<u8>(), any::<u16>()),
      1..if cfg!(miri) { 7 } else { 60 },
    )
  ) {
    let mut store = store();
    let mut volume = volume(&mut store, 1 << 22);
    let files: Vec<InodeNo> = (0..CHAOS_FILES)
      .map(|i| {
        let fill = 0x10u8.saturating_add(u8::try_from(i).unwrap_or(0));
        file_of(&mut store, &mut volume, &format!("f{i}"), fill, DAX_PAGE)
      })
      .collect();
    let mut held: Vec<Mapping> = Vec::new();

    for step in schedule {
      chaos_step(&mut store, &mut volume, &files, &mut held, step)?;
      chaos_invariants(&store, &volume, &held)?;
      // Drain what the step revoked, as the device's serve loop drains pending revokes: a dead mapping's block
      // stays pinned until the unmap is confirmed, and leaving them held would fill the arena instead of
      // exercising the gate.
      let dead: Vec<Mapping> = held
        .iter()
        .filter(|mapping| !store.dax_is_live(mapping))
        .copied()
        .collect();
      for mapping in &dead {
        store.dax_release_after_unmap(mapping);
      }
      held.retain(|mapping| store.dax_is_live(mapping));
    }
  }
}

/// One step of the §4.6 layered-defence schedule: the operations L1, L2 and B2 fight each other with.
fn layers_step(
  store: &mut Store,
  volume: &mut Volume,
  files: &[InodeNo],
  held: &mut Vec<Mapping>,
  (op, which, size): (u8, u8, u16),
) -> Result<(), TestCaseError> {
  let no = files[usize::from(which) % files.len()];
  let len = usize::from(size) % (DAX_PAGE * 2);
  match op % 9 {
    0 => {
      let _ = volume.write(store, no, 0, &vec![op ^ which; len.max(1)]);
    }
    1 => {
      let _ = volume.truncate(store, no, len as u64);
    }
    2 => {
      // A read map, which the volume's opt-in guards (B2).
      if let Ok(mapping) = volume.dax_map_read(store, no) {
        held.push(mapping);
      }
    }
    3 => {
      if let Ok(mapping) = volume.dax_map_write(store, no) {
        held.push(mapping);
      }
    }
    4 => {
      if let Some(mapping) = held.pop() {
        store.dax_revoke(&mapping);
      }
    }
    5 => {
      // L2's trigger, one slice, as the shard's loop runs it.
      let due = store.dax.writable_of_prefix(volume.prefix());
      let _ = volume.dax_shift_slice(store, &due, 0, u64::from(size).max(1));
    }
    6 => {
      // B2's net, one slice, as the reap tick runs it.
      let slice = volume.dax_scrub_slice(store, 0, u64::from(size).max(1));
      // Nothing in this schedule corrupts a block, so the net must never claim it found a flip. A heal here
      // would mean a legitimate operation reached a guarded block without dropping its guard first — which
      // would make B2 *revert* that operation, the worst thing a self-healing net can do.
      prop_assert_eq!(
        slice.healed,
        0,
        "B2 healed a block nothing corrupted: a legitimate write reached a guarded block with its guard still on"
      );
      prop_assert_eq!(
        slice.unhealable,
        0,
        "B2 reported an unhealable block in a schedule that corrupts nothing"
      );
    }
    7 => {
      // The churn that takes a row out of the sandbox: the tenant's own delete, then a fresh file in its place.
      if let Ok(root) = volume.root_inode(&*store) {
        let name = format!("f{}", usize::from(which) % files.len());
        let _ = volume.unlink_no(store, root, &name);
      }
    }
    _ => {
      volume.age_writes();
      let _ = volume.seal_pending(store, 1 << 20);
    }
  }
  Ok(())
}

/// The sandbox's invariants after every step of the layered schedule.
fn layers_invariants(store: &Store, volume: &Volume) -> Result<(), TestCaseError> {
  let Some(zone) = store.dax_zones.of_volume(volume.prefix()) else {
    return Ok(());
  };
  let (zone_held, slots) = zone.held();
  prop_assert!(
    zone_held <= slots,
    "the sandbox cannot hold more rows than it was cut into: {zone_held} of {slots}"
  );
  // Every live writable window of this volume that sits in the sandbox sits on a row the sandbox is *not*
  // holding — the zone lends a row or holds it, never both, or two tenants could be handed the same row.
  for mapping in store.dax.writable_of_prefix(volume.prefix()) {
    if let Some(index) = zone.slot_of(mapping.block) {
      prop_assert!(
        !zone.holds_slot(index),
        "the sandbox is holding row {index} while a live window sits on it"
      );
    }
  }
  Ok(())
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(if cfg!(miri) { 4 } else { 80 }), .. ProptestConfig::default() }))]

  /// §4.6's **four layers under chaos together** — the interleavings nobody writes by hand. Every other test
  /// here drives one layer; this one runs L0's opt-ins, L1's sandbox, L2's periodic shift and B2's scrub
  /// against each other while the ordinary content paths churn underneath: writes and truncates that must
  /// revoke, deletes that take a row out of the sandbox, the sealing sweep, maps read and writable.
  ///
  /// Expect, after **every** step: a live, current mapping reads its own bytes and zeroes past its file's
  /// length (the gate's standing invariant); the sandbox never holds a row a live window sits on, which is the
  /// property that keeps two tenants off one row; and — the sharp one — **B2 never heals anything**, because
  /// nothing in this schedule corrupts a block. A heal here would mean a legitimate operation reached a
  /// guarded block without dropping its guard, so the net would *revert* that operation: the worst failure a
  /// self-healing net has.
  ///
  /// Any step may be refused; a refusal is a valid outcome, and on a host that backs no sandbox the shift
  /// refuses every time and the rest of the schedule still has to hold.
  #[test]
  fn the_four_layers_under_chaos_never_heal_what_nothing_corrupted(
    schedule in prop::collection::vec(
      (any::<u8>(), any::<u8>(), any::<u16>()),
      1..if cfg!(miri) { 7 } else { 40 },
    )
  ) {
    let mut store = store_sandboxed();
    let mut volume = volume(&mut store, 1 << 22);
    volume.enable_dax_integrity_guard();
    let files: Vec<InodeNo> = (0..CHAOS_FILES)
      .map(|i| {
        let fill = 0x10u8.saturating_add(u8::try_from(i).unwrap_or(0));
        file_of(&mut store, &mut volume, &format!("f{i}"), fill, DAX_PAGE)
      })
      .collect();
    let mut held: Vec<Mapping> = Vec::new();

    for step in schedule {
      layers_step(&mut store, &mut volume, &files, &mut held, step)?;
      chaos_invariants(&store, &volume, &held)?;
      layers_invariants(&store, &volume)?;
      let dead: Vec<Mapping> = held
        .iter()
        .filter(|mapping| !store.dax_is_live(mapping))
        .copied()
        .collect();
      for mapping in &dead {
        store.dax_release_after_unmap(mapping);
        // A row the sandbox is holding **free** belongs to nobody — no mapping, no inode, no snapshot — so it
        // must carry none of the tenant's bytes. This is the vacated-row scrub as a property rather than the
        // single hand-built case: a shift hands the row the content left back to this tenant's own zone
        // instead of to the arena's free lists, and only the free path scrubs, so before
        // `Store::scrub_vacated_sandbox_row` every shift in this schedule left a copy behind. The condition is
        // the zone's own "holds it free" and not "is no longer current", because a block a snapshot still
        // references is legitimately not current and legitimately keeps its bytes.
        let held_free = store
          .dax_zones
          .of_volume(volume.prefix())
          .and_then(|zone| zone.slot_of(mapping.block).map(|slot| zone.holds_slot(slot)))
          .unwrap_or(false);
        // ...unless the row is still pinned. Between a shift's `return_slot` and the VMM's confirmed unmap the
        // zone holds the row free *and* its bytes are deliberately kept: that is the stale-TLB hold, and the
        // scrub waits for it on purpose. Only an unpinned row is one nobody can still be reading.
        if held_free
          && !store.content.arena().is_pinned(mapping.block)
          && let Some(index) = files.iter().position(|no| *no == mapping.inode)
          && let Some(bytes) = store.content.arena().bytes(mapping.block)
        {
          let fill = 0x10u8.saturating_add(u8::try_from(index).unwrap_or(0));
          prop_assert!(
            !bytes.contains(&fill),
            "a row the sandbox holds free still carries file byte {fill:#04x}: the content left this row \
             and its bytes stayed on it"
          );
        }
      }
      held.retain(|mapping| store.dax_is_live(mapping));
    }
  }
}

/// §4.6 B2 (the design's words: a refused guard leaves "the mapping served unguarded: the net is under the map,
/// never a condition of it"). Do: fill the shard's whole chunk budget with file content so the net has no room for
/// another clean copy, then map a read-only window on a volume that asked for the guard. Expect: the window is
/// **served** — a guest entitled to a mapping is never failed because the safety net is full — the guard count stays
/// where it was, `guards_refused` climbs by exactly one, and the window reads the file's real bytes.
///
/// This is the first test to read `DaxRegistry::guards_refused`. Before it, the counter had **no caller anywhere in
/// the workspace** — written at `guard_if_asked` and read by nothing — so the refusal branch had never executed in a
/// test, and a net that silently refused every guard was indistinguishable from one that never filled. That is the
/// exact shape CLAUDE.md's non-vacuity rule exists to catch, on the counter the design nominates as the capacity
/// signal: "a count climbing beside a flat guard count says the shard is at its net's bound, which is a capacity
/// signal, not a bug".
#[test]
fn a_full_guard_net_still_serves_the_window_and_counts_the_refusal() {
  const CAP: usize = 8;
  const FILL: u8 = 0x5a;
  /// A whole content chunk, so the fill loop spends the shard's chunk budget rather than packing
  /// several small files into one chunk (a 4 KiB file did not exhaust it, measured 2026-10-20).
  const WHOLE_CHUNK: usize = 64 * 1024;
  /// Enough attempts to reach the arena's end at `WHOLE_CHUNK` a time (the test region is 16 MiB), with room
  /// to spare so the loop ends on a refusal and never on its own bound — which the assertion below checks.
  const FILL_ATTEMPTS: usize = 1024;
  let mut store = store_capped_chunks(CAP);
  let mut volume = volume(&mut store, 1 << 25);
  volume.enable_dax_integrity_guard();

  // Spend the shard's whole content arena, so the net cannot allocate another clean copy. `max_chunks` bounds
  // the DAX registry, not content allocation — measured 2026-10-20, 32 files of 64 KiB went in under
  // `store_capped_chunks(8)` without a refusal — so the fill has to reach the arena's own region, and it stops
  // at the first write the arena cannot take rather than assuming how far that is.
  let mut inodes = Vec::new();
  for index in 0..FILL_ATTEMPTS {
    let root = volume.root_inode(&store).unwrap();
    let name = format!("f{index}");
    let Ok(no) = volume.create_file_no(&mut store, root, &name, 0o644) else {
      break;
    };
    if volume
      .write(&mut store, no, 0, &vec![FILL; WHOLE_CHUNK])
      .is_err()
    {
      break;
    }
    inodes.push(no);
  }
  assert!(
    inodes.len() >= 2 && inodes.len() < FILL_ATTEMPTS,
    "the fill ended because the arena was full, not because the loop ran out: {} files",
    inodes.len()
  );

  let guarded_before = store.dax.guards();
  let refused_before = store.dax.guards_refused();
  let first = inodes[0];
  let mapping = volume
    .dax_map_read(&mut store, first)
    .expect("a window is served even when the net cannot take a clean copy for it");

  assert!(
    store.dax.is_live(&mapping),
    "the mapping is live, not a half-made one"
  );
  assert_eq!(
    store.dax.guards(),
    guarded_before,
    "the net took no clean copy: it had no room for one"
  );
  assert_eq!(
    store.dax.guards_refused(),
    refused_before.saturating_add(1),
    "and the refusal is counted exactly once, which is the capacity signal the design names"
  );

  // Served unguarded still means served correctly.
  let seen = store.dax_read_raw(&mapping, 0, DAX_PAGE);
  assert_eq!(
    seen,
    vec![FILL; DAX_PAGE],
    "the unguarded window serves the file's real bytes"
  );
}

/// §4.6 B2 — the guard bound, measured instead of reasoned about. `dax_guard` refuses `NoSpace` when
/// `guards.len() >= cap`, and the obvious question is whether that arm can ever be reached, since a guard
/// exists only for a live mapping and mappings are bounded by the *same* `cap`. Do: fill the registry to its
/// cap with guarded read-only mappings, then (a) ask for one more window and (b) re-guard a mapping that
/// already has a guard. Expect: (a) is refused by the registry's own bound in `register`, before
/// `guard_if_asked` is ever consulted — which is why the production path never reaches the guard arm — and
/// (b) is refused `NoSpace` by the guard arm, even though a refresh frees the old clean copy before it
/// records the new one, so the net would not grow.
///
/// Recorded because the previous entry in the ledger called this arm "unreachable" on reasoning alone. It is
/// reachable, through the public `Store::dax_guard` on an already-guarded mapping, and the refusal there sits
/// against `dax_guard`'s own words — "Re-guarding a mapping refreshes it (the old clean copy is freed)". No
/// production caller does that today (`guard_if_asked` only ever guards a freshly minted mapping, which has no
/// guard yet), so the behaviour is pinned here rather than changed: a speculative fix to a path nothing walks
/// would be the same mistake in the other direction.
#[test]
fn the_guard_bound_is_reached_only_by_re_guarding_and_the_registry_refuses_first() {
  const CAP: usize = 8;
  const FILL: u8 = 0x3c;
  let mut store = store_capped_chunks(CAP);
  let mut volume = volume(&mut store, 1 << 22);
  volume.enable_dax_integrity_guard();

  let mut mappings = Vec::new();
  for index in 0..CAP {
    let no = file_of(
      &mut store,
      &mut volume,
      &format!("g{index}"),
      FILL,
      DAX_PAGE,
    );
    mappings.push(
      volume
        .dax_map_read(&mut store, no)
        .expect("each window up to the cap is served"),
    );
  }
  assert_eq!(
    store.dax.guards(),
    CAP,
    "every mapping up to the cap took a guard, so the net is exactly full"
  );
  assert_eq!(store.dax_mappings(), CAP, "and so is the registry");

  // (a) One more window: the registry's bound answers first, so `guard_if_asked` never runs.
  let refused_before = store.dax.guards_refused();
  let one_more = file_of(&mut store, &mut volume, "overflow", FILL, DAX_PAGE);
  assert!(
    matches!(
      volume.dax_map_read(&mut store, one_more),
      Err(VfsError::NoSpace)
    ),
    "the registry refuses at its own bound"
  );
  assert_eq!(
    store.dax.guards_refused(),
    refused_before,
    "and the net was never asked, so no guard refusal was counted: this is why the production \
     path cannot reach the guard arm"
  );

  // (b) A refresh of a guard that already exists: the guard arm, reached.
  let existing = mappings[0];
  assert!(
    matches!(store.dax_guard(&existing), Err(VfsError::NoSpace)),
    "a re-guard at the bound is refused, though it would free the old clean copy before recording \
     the new one"
  );
  assert_eq!(
    store.dax.guards(),
    CAP,
    "and the refusal left the net exactly as it was"
  );
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(if cfg!(miri) { 4 } else { 96 }), .. ProptestConfig::default() }))]

  /// §4.6 B2 under arbitrary corruption, which is the shape the threat actually has. Every heal test above
  /// corrupts **byte 0** — `flip_a_byte` is `bytes[0] ^= 0xFF` — and the one that checks the repair reads back
  /// only the first 64 bytes. A Rowhammer flip lands wherever the aggressor rows put it, and a heal that
  /// restored a prefix would pass every one of those tests.
  ///
  /// Do: for an arbitrary fill byte, an arbitrary position anywhere in the mapped block (its zeroed tail past
  /// the file's length included) and an arbitrary non-zero mask, guard a read-only mapping, flip that byte, and
  /// scrub. Expect: the scrub reports `Healed`, and **every** byte of the block equals what it was before the
  /// corruption — compared whole, reported by first differing index so a failure is readable rather than a
  /// 64 KiB diff.
  #[test]
  fn a_flip_anywhere_in_a_guarded_block_heals_every_byte(
    fill in any::<u8>(),
    position in any::<u16>(),
    mask in 1u8..=u8::MAX,
  ) {
    let mut store = store();
    let mut volume = volume(&mut store, 1 << 20);
    let f = file_of(&mut store, &mut volume, "f", fill, DAX_PAGE);
    let m = volume.dax_map_read(&mut store, f).unwrap();
    store.dax_guard(&m).unwrap();
    prop_assert_eq!(
      store.dax_scrub(&m),
      DaxScrub::Clean,
      "a freshly guarded block scrubs clean, so anything found below is the corruption and not the fixture"
    );

    let Some(before) = store.content.arena().bytes(m.block).map(<[u8]>::to_vec) else {
      return Err(TestCaseError::reject("the mapped block has no bytes to corrupt"));
    };
    prop_assume!(!before.is_empty());
    let at = usize::from(position) % before.len();
    let was = before[at];
    poison_a_byte(&mut store, m.block, at, was ^ mask);

    prop_assert_eq!(
      store.dax_scrub(&m),
      DaxScrub::Healed,
      "a flip at byte {} of {} is caught wherever it lands",
      at,
      before.len()
    );
    let Some(after) = store.content.arena().bytes(m.block).map(<[u8]>::to_vec) else {
      return Err(TestCaseError::fail("the block vanished across the heal"));
    };
    prop_assert_eq!(after.len(), before.len(), "the heal kept the block's length");
    let first_difference = before
      .iter()
      .zip(after.iter())
      .position(|(a, b)| a != b);
    prop_assert_eq!(
      first_difference,
      None,
      "the heal restored every byte, not a prefix: corrupted byte {} of {}, first byte still wrong {:?}",
      at,
      before.len(),
      first_difference
    );
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(if cfg!(miri) { 4 } else { 96 }), .. ProptestConfig::default() }))]

  /// §4.6 — the disclosure boundary, at every length rather than one. A mapping hands the guest a whole page
  /// whatever the file's length, so the bytes past that length must read as zero or the guest sees what a
  /// truncate took away. `a_mapping_does_not_expose_the_bytes_past_the_files_length` proves this for a file
  /// truncated to **100** bytes; an off-by-one, or a scrub that started at the next word or page boundary,
  /// would leak at some other length and pass that test.
  ///
  /// Do: write a page of a secret byte, truncate to an arbitrary length, map read-only, read the whole page.
  /// Expect: no byte at or past the file's length is the secret, and every byte before it still is — the tail
  /// is hidden without the kept head being damaged.
  #[test]
  fn a_mapping_hides_the_tail_at_whatever_length_the_file_was_truncated_to(
    len in 0usize..DAX_PAGE,
  ) {
    let mut store = store();
    let mut volume = volume(&mut store, 1 << 20);
    let secret = file_of(&mut store, &mut volume, "secret", SECRET, DAX_PAGE);
    prop_assert!(
      volume
        .truncate(&mut store, secret, u64::try_from(len).unwrap_or(0))
        .is_ok(),
      "the truncate to {} is accepted",
      len
    );

    let mapping = match volume.dax_map_read(&mut store, secret) {
      Ok(mapping) => mapping,
      Err(refusal) => {
        return Err(TestCaseError::fail(format!(
          "a read map of a {len}-byte file was refused: {refusal:?}"
        )));
      }
    };
    prop_assert_eq!(
      mapping.file_len,
      len,
      "the authorized length is the truncated size"
    );
    let page = volume
      .dax_read(&store, &mapping, 0, DAX_PAGE)
      .map_err(|e| TestCaseError::fail(format!("the window reads: {e:?}")))?;

    let leak = page
      .get(len..)
      .and_then(|tail| tail.iter().position(|&b| b == SECRET));
    prop_assert_eq!(
      leak,
      None,
      "a byte of the truncated-away secret is visible {:?} past a length of {}",
      leak,
      len
    );
    prop_assert!(
      page.get(..len).is_some_and(|head| head.iter().all(|&b| b == SECRET)),
      "and the kept head of {} bytes is intact",
      len
    );
  }
}

/// §4.6 L2 — what a shift leaves on the row it vacated, measured rather than assumed. The existing shift tests
/// assert the arena's allocation accounting and never look at the vacated bytes, so this asks directly.
///
/// Do: map a file writable, shift it onto another row of the sandbox, complete the two-phase release of the old
/// mapping, and interrogate the row it left. Expect: before the release its bytes are still there — that is the
/// stale-TLB hold, and a scrub *there* would be a use-after-unmap hazard rather than a fix; after it, the row is
/// not handed to anyone else — the arena's allocated byte count does not move and a fresh allocation of the same
/// size gets a different block carrying none of the tenant's bytes; and the content is intact where it moved to.
///
/// **What this deliberately does not assert is that the vacated row is scrubbed, because it is not.** The first
/// draft asserted exactly that and failed on Linux with the secret still at byte 0. The reason is in the design:
/// the row is a slot of this tenant's own sandbox (`slot_of` answers `Some`), so `DaxZones::return_slot`
/// takes it back into the zone instead of freeing it — "so no neighbour can take a row inside this tenant's page
/// while the tenant still writes the page" — and only the arena's free path scrubs. Measured on Linux
/// 2026-10-20: same-tenant retention, not cross-tenant disclosure. That an unsealed copy of the tenant's
/// plaintext stays on the vacated row for the life of the sandbox is recorded in GAPS as a finding, not fixed
/// here.
///
/// Skips loudly on a host that backs no transparent huge page, as every sandbox test here does.
#[test]
fn a_shift_leaves_no_copy_of_the_content_on_the_row_it_vacated() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", SECRET, DAX_PAGE);
  let m = volume.dax_map_write(&mut store, f).unwrap();
  let vacated = m.block;

  let Some(moved) = first_shift(
    &mut store,
    &mut volume,
    &m,
    "a_shift_leaves_no_copy_of_the_content_on_the_row_it_vacated",
  ) else {
    return;
  };
  assert_ne!(
    moved.block, vacated,
    "the shift put the content on a different block, or there is nothing to vacate"
  );

  // Phase one only: the old block's bytes are held on purpose until the VMM confirms the unmap, because a guest
  // TLB entry may still point at them. A scrub here would be a use-after-unmap hazard, not a fix.
  let held = store.content.arena().bytes(vacated).map(<[u8]>::to_vec);
  assert!(
    held.is_some_and(|b| b.contains(&SECRET)),
    "before the release the vacated row still holds its bytes: that is the stale-TLB hold"
  );

  let allocated_before = store.content.arena().allocated_bytes();
  assert!(
    store.dax_release_after_unmap(&m),
    "the hold is dropped by the second phase"
  );
  // Which slot it is depends on the host's page size and the zone's layout, so the assertion is that it *is*
  // one — the index itself would be a constant of this machine, not of slates.
  let slot = store
    .dax_zones
    .of_volume(volume.prefix())
    .and_then(|zone| zone.slot_of(vacated));
  assert!(
    slot.is_some(),
    "the row the content left is a slot of this tenant's own sandbox, which is why it goes back to the \
     sandbox rather than to the arena's free lists; slot_of said {slot:?}"
  );

  // The row is not handed to anyone else: it went back to this tenant's own sandbox, not to the free lists.
  assert_eq!(
    store.content.arena().allocated_bytes(),
    allocated_before,
    "the release returned nothing to the arena's free lists"
  );
  let fresh = store
    .content
    .alloc_block(DAX_PAGE, false)
    .expect("a fresh block is available");
  assert_ne!(
    fresh, vacated,
    "a later allocation is not handed the row the content left"
  );
  assert!(
    store
      .content
      .arena()
      .bytes(fresh)
      .is_some_and(|b| b.iter().all(|&x| x != SECRET)),
    "and whatever it is handed carries none of the tenant's bytes"
  );

  // And the row carries no copy of what left it: a moving target that keeps the bytes on the row it
  // vacated has duplicated the tenant's plaintext onto a second physical row rather than left one.
  let vacated_bytes = store.content.arena().bytes(vacated);
  assert!(
    vacated_bytes.is_some_and(|b| !b.contains(&SECRET)),
    "the vacated row is scrubbed once its stale-TLB hold is gone; first secret byte at {:?}",
    vacated_bytes.and_then(|b| b.iter().position(|&x| x == SECRET))
  );

  let now = store.dax_read_raw(&moved, 0, DAX_PAGE);
  assert!(
    now.iter().take(DAX_PAGE).all(|&b| b == SECRET),
    "and the content is intact where it moved to"
  );
}

/// §4.6 **L1** — whether a volume gets a sandbox in the shared object a daemon serves DAX from is decided by what
/// this host backs, never by where `mmap` happened to put the object.
///
/// Do: build several independent stores, each over its own shared memory object exactly as the daemon's content
/// pool does (`server/src/content_pool.rs` maps every content region from the anchor's object), and ask each for
/// the first shift. Expect: every one of them answers the same way — all reserved, or all refused. A host either
/// backs huge pages for a memory object or it does not, and that answer cannot differ between two objects a few
/// microseconds apart.
///
/// Measured 2026-10-20 on Linux 6.12.76 (aarch64): outcomes disagreed, 3 of 16 mappings landing on a huge-page
/// boundary and the rest not, so the same reservation was accepted or refused by address luck. The buddy hands a
/// huge-sized allocation out at a huge-aligned *offset*, but the span's address is `region.base() + offset`, and a
/// memory object's mapping begins on a 64 KiB boundary rather than a huge one — the kernel aligns a mapping to a
/// huge page only for an anonymous span, or for an object where shmem huge pages are on. The private fixture is
/// aligned for free and hid it.
#[test]
fn a_sandbox_in_a_shared_object_is_decided_by_the_host_not_by_where_mmap_put_the_object() {
  use slates_mem::arena::ChunkArena;
  use slates_mem::region::Region;
  use slates_mem::shared::{ExclusiveObject, SharedObject};
  use slates_mem::words::Words;
  use slates_vfs::volume::StoreConfig;

  let page = 4096usize;
  // Derived: as the private fixture — eight of the host's huge pages, so the sandbox, the content and the arena's
  // rounding all fit without the fixture's size shaping the result.
  const SANDBOX_HUGE_PAGES: usize = 8;
  // Derived: enough objects that one address landing aligned by chance cannot make the run look unanimous. At
  // this host's 64 KiB granularity one address in 32 is huge-aligned, so eight objects agreeing by luck is one
  // run in a million.
  const OBJECTS: usize = 8;
  let Some(huge) =
    slates_machine::facts::huge_page_bytes().and_then(|bytes| usize::try_from(bytes).ok())
  else {
    println!(
      "SKIP a_sandbox_in_a_shared_object_is_decided_by_the_host_not_by_where_mmap_put_the_object: this host \
       has no transparent huge page, so every object is refused alike"
    );
    return;
  };

  let mut reserved = 0usize;
  let mut addresses = Vec::new();
  for sample in 0..OBJECTS {
    let object = SharedObject::create(
      &format!("sl-dax-luck-{}-{sample}", std::process::id()),
      huge * SANDBOX_HUGE_PAGES,
      Words::new(),
    )
    .unwrap();
    let mut arena = ChunkArena::new(page);
    // SAFETY: this test is the object's only mapper, and it declares no words.
    let region = Region::shared(unsafe { ExclusiveObject::new(object) }, page);
    addresses.push(region.base() as usize);
    arena.add_region(region).unwrap();
    let mut store = Store::new(
      &StoreConfig {
        page,
        cache_line: 128,
        max_dirs: 1 << 10,
        max_inodes: 1 << 12,
        max_chunks: 1 << 12,
        max_dir_blocks: 1 << 10,
        dir_cutover: 16,
      },
      arena,
      0,
    );
    let mut volume = volume(&mut store, 1 << 20);
    let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
    let m = volume.dax_map_write(&mut store, f).unwrap();
    match volume.dax_migrate(&mut store, &m) {
      Ok(_) => reserved = reserved.saturating_add(1),
      Err(VfsError::NoPhysicalIsolation) => {}
      Err(other) => {
        panic!("a shift is either performed or refused for want of a sandbox: {other:?}")
      }
    }
  }
  println!(
    "{reserved} of {OBJECTS} shared objects were given a sandbox; object addresses {addresses:#x?}"
  );
  assert!(
    reserved == 0 || reserved == OBJECTS,
    "every shared object answers alike, because the host's backing decides and not the address `mmap` chose: \
     {reserved} of {OBJECTS} were reserved, addresses {addresses:#x?} (huge page {huge})"
  );
}
