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
use common::{clone_config, store, store_capped_chunks};

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

/// §4.6 multi-tenancy **L2** (the moving target; the `dax_migrate` stripe-shift primitive the periodic Rowhammer
/// defence composes from). Do: map a file writable, write a known marker the hardware way, migrate it. Expect: the
/// content moves to a *physically different* block with every byte preserved; the inode follows, so the daemon path
/// and a plain non-DAX read both reach the moved bytes; the new mapping is live, the old is inert; the counter moved.
#[test]
fn a_migration_moves_the_content_to_a_new_block_and_repoints_the_inode() {
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let m = volume.dax_map_write(&mut store, f).unwrap();
  store.dax_write_raw(&m, 0, &[0x55; 8]); // a known marker written through the guest's own page table

  let m2 = volume.dax_migrate(&mut store, &m).unwrap();

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
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);
  let m = volume.dax_map_write(&mut store, f).unwrap();
  let old_block = m.block;

  let _m2 = volume.dax_migrate(&mut store, &m).unwrap();

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
  use slates_vfs::error::VfsError;
  let mut store = store();
  let mut volume = volume(&mut store, 1 << 20);
  let f = file_of(&mut store, &mut volume, "f", 0x41, DAX_PAGE);

  let ro = volume.dax_map_read(&mut store, f).unwrap();
  assert!(
    matches!(volume.dax_migrate(&mut store, &ro), Err(VfsError::Invalid)),
    "a read-only mapping is not migrated"
  );
  assert_eq!(store.dax.migrated(), 0, "nothing migrated");

  // Non-vacuity: the same file, mapped writable, does migrate.
  store.dax_revoke(&ro);
  let rw = volume.dax_map_write(&mut store, f).unwrap();
  assert!(
    volume.dax_migrate(&mut store, &rw).is_ok(),
    "a writable mapping of the same file migrates"
  );
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
      if let Some(mapping) = held.pop() {
        store.dax_revoke(&mapping);
        store.dax_release_after_unmap(&mapping);
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
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(150), .. ProptestConfig::default() }))]

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
    schedule in prop::collection::vec((any::<u8>(), any::<u8>(), any::<u16>()), 1..60)
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
