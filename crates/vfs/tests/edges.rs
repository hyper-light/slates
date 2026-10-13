//! The edge, error, fault and amplification tests of Phase 1 (design Part 5, Phase 1): each
//! drives the volume through its public API and asserts observable behaviour.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{PAGE, dynamic_volume, store, store_with, volume, volume_with};
use slates_vfs::dir::Child;
use slates_vfs::error::VfsError;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, Volume};

/// Creates `entry-NNNNNN` for every number in `range` under `dir`.
fn fill(
  vol: &mut Volume,
  store: &mut Store,
  dir: slates_mem::Handle<slates_vfs::dir::DirNode>,
  range: std::ops::Range<usize>,
) {
  for n in range {
    vol
      .create_file(store, dir, &format!("entry-{n:06}"), 0o644)
      .unwrap();
  }
}

/// Unlinks `entry-NNNNNN` for every number in `range` under `dir`.
fn drain(
  vol: &mut Volume,
  store: &mut Store,
  dir: slates_mem::Handle<slates_vfs::dir::DirNode>,
  range: std::ops::Range<usize>,
) {
  for n in range {
    vol.unlink(store, dir, &format!("entry-{n:06}")).unwrap();
  }
}

/// The names a directory lists, in the order the volume returns them.
fn listing(
  vol: &Volume,
  store: &Store,
  dir: slates_mem::Handle<slates_vfs::dir::DirNode>,
) -> Vec<String> {
  vol
    .readdir(store, dir)
    .unwrap()
    .iter()
    .map(|r| r.name.to_string())
    .collect()
}

/// T-1.2: names that fold equal under the policy are one entry: `README` and `readme`, and the
/// precomposed and decomposed spellings of `é`; the second create is `EEXIST` and lookups by
/// either spelling find the one entry.
#[test]
fn names_that_fold_equal_are_one_entry() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root();
  let upper = vol.create_file(&mut store, root, "README", 0o644).unwrap();
  assert_eq!(
    vol.create_file(&mut store, root, "readme", 0o644),
    Err(VfsError::AlreadyExists)
  );
  assert_eq!(vol.lookup(&store, root, "ReadMe").unwrap().inode, upper);
  let composed = vol
    .create_file(&mut store, root, "caf\u{e9}", 0o644)
    .unwrap();
  assert_eq!(
    vol.create_file(&mut store, root, "cafe\u{301}", 0o644),
    Err(VfsError::AlreadyExists)
  );
  assert_eq!(
    vol.lookup(&store, root, "CAFE\u{301}").unwrap().inode,
    composed
  );
  let names = listing(&vol, &store, root);
  assert_eq!(names.len(), 2, "one entry per folded name: {names:?}");

  // An exact-policy volume keeps them apart.
  let mut exact = volume_with(
    &mut store,
    Quota::Bounded { limit: 1 << 20 },
    NameEquivalence::Exact,
  );
  let root = exact.root();
  exact
    .create_file(&mut store, root, "README", 0o644)
    .unwrap();
  exact
    .create_file(&mut store, root, "readme", 0o644)
    .unwrap();
  assert_eq!(exact.readdir(&store, root).unwrap().len(), 2);
}

/// T-1.3: a write at offset 10 GiB in a dynamic volume charges one window holding one byte —
/// the one page-multiple block it takes, never the hole (§4.2 "physical_used includes allocator
/// rounding": the charge is the arena block, so the content memory taken and the bytes charged
/// are one number) — and holes read as zeros.
#[test]
fn a_sparse_write_in_a_dynamic_volume_charges_one_chunk() {
  let mut store = store();
  let mut vol = dynamic_volume(&mut store, 1 << 40, 1 << 40);
  let root = vol.root();
  let f = vol.create_file(&mut store, root, "sparse", 0o644).unwrap();
  let far = 10u64 << 30;
  vol.write(&mut store, f, far, b"x").unwrap();
  assert_eq!(vol.stat(&store, f).unwrap().size, far + 1);
  let mut buf = [7u8; 8];
  assert_eq!(vol.read(&store, f, far - 4, &mut buf).unwrap(), 5);
  assert_eq!(&buf[..5], &[0, 0, 0, 0, b'x']);
  assert_eq!(
    vol.accounting().referenced_bytes,
    u64::try_from(PAGE).unwrap(),
    "one window, holding the one byte written at a window start, charged its one-page block"
  );
  assert_eq!(
    store.content.allocated_bytes(),
    PAGE,
    "one page-multiple block: the bytes charged are the bytes taken"
  );
}

/// T-1.5: growth denied by the pressure source is `ENOSPC` with a pressure event and no
/// partial write: the size and the bytes are unchanged.
#[test]
fn growth_denied_by_the_pressure_source_is_enospc_with_an_event_and_no_partial_write() {
  let mut store = store();
  let mut vol = dynamic_volume(&mut store, 1 << 30, 100);
  let root = vol.root();
  let f = vol.create_file(&mut store, root, "f", 0o644).unwrap();
  assert_eq!(
    vol.write(&mut store, f, 0, &[1u8; 200]),
    Err(VfsError::NoSpace)
  );
  assert_eq!(VfsError::NoSpace.errno_name(), "ENOSPC");
  assert_eq!(vol.growth_denials(), 1, "one pressure event");
  assert_eq!(vol.stat(&store, f).unwrap().size, 0);
  let mut buf = [0u8; 8];
  assert_eq!(vol.read(&store, f, 0, &mut buf).unwrap(), 0);
  vol.write(&mut store, f, 0, &[2u8; 60]).unwrap();
  assert_eq!(
    vol.write(&mut store, f, 60, &[3u8; 50]),
    Err(VfsError::NoSpace)
  );
  assert_eq!(vol.growth_denials(), 2);
  assert_eq!(
    vol.stat(&store, f).unwrap().size,
    60,
    "the failed append changed nothing"
  );
  assert_eq!(vol.accounting().referenced_bytes, 60);
}

/// T-1.8: a directory of 61,067 entries lists in canonical order, every name looks up, and
/// after shrinking it back below the cut-over the order is the same and the representation is
/// the small one again.
#[test]
fn a_directory_of_61067_entries_lists_canonically_and_looks_up_every_name() {
  /// Format: the design's entry count for this case.
  const ENTRIES: usize = 61_067;
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root();
  let dir = vol.mkdir(&mut store, root, "big", 0o755).unwrap();
  fill(&mut vol, &mut store, dir, 0..ENTRIES);
  let listed = listing(&vol, &store, dir);
  assert_eq!(listed.len(), ENTRIES);
  let policy = vol.policy();
  let mut canonical = listed.clone();
  canonical.sort_by_key(|n| (policy.hash(n), n.clone()));
  assert_eq!(listed, canonical, "readdir order is (hash, name)");
  assert!(store.dirs.get(dir).unwrap().is_indexed());
  for n in (0..ENTRIES).step_by(997) {
    assert!(vol.lookup(&store, dir, &format!("ENTRY-{n:06}")).is_ok());
  }
  // Back below half the cut-over (the hysteresis of §4.5) the small representation returns.
  drain(&mut vol, &mut store, dir, 1..ENTRIES);
  assert!(!store.dirs.get(dir).unwrap().is_indexed(), "shrunk back");
  assert_eq!(listing(&vol, &store, dir), vec!["entry-000000".to_owned()]);
}

/// T-1.9: an allocator refusal in the middle of a rename fails the rename atomically with a
/// typed refusal; both names are as they were.
#[test]
fn an_allocator_refusal_during_rename_fails_atomically_and_typed() {
  // Three directory slots: the root, `a` and `b`; the copies a post-snapshot rename needs do
  // not fit.
  let mut store = store_with(3, 4);
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root();
  let a = vol.mkdir(&mut store, root, "a", 0o755).unwrap();
  let b = vol.mkdir(&mut store, root, "b", 0o755).unwrap();
  vol.create_file(&mut store, a, "inside", 0o644).unwrap();
  vol.snapshot(&mut store).unwrap();
  let refusal = vol.rename(&mut store, root, "a", b, "x");
  assert!(
    matches!(refusal, Err(VfsError::Memory(_))),
    "typed memory refusal, got {refusal:?}"
  );
  assert_eq!(refusal.unwrap_err().errno_name(), "ENOMEM");
  assert_both_names_intact(&vol, &store);
}

/// `/a` (with its file) and `/b` are directories, `/b/x` does not exist, the root lists two.
fn assert_both_names_intact(vol: &Volume, store: &Store) {
  let root = vol.root();
  let is_dir = |name: &str| matches!(vol.lookup(store, root, name).unwrap().child, Child::Dir(_));
  assert!(is_dir("a"));
  assert!(is_dir("b"));
  assert_eq!(vol.resolve(store, "/b/x"), Err(VfsError::NotFound));
  assert!(vol.resolve(store, "/a/inside").is_ok());
  assert_eq!(listing(vol, store, root).len(), 2);
}

/// A five-deep directory holding a three-page file, snapshotted.
fn deep_tree_snapshotted() -> (
  slates_vfs::volume::Store,
  Volume,
  slates_mem::Handle<slates_vfs::dir::DirNode>,
  slates_vfs::ids::InodeNo,
) {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let mut dir = vol.root();
  for name in ["a", "b", "c", "d"] {
    dir = vol.mkdir(&mut store, dir, name, 0o755).unwrap();
  }
  let f = vol.create_file(&mut store, dir, "f", 0o644).unwrap();
  vol.write(&mut store, f, 0, &vec![9u8; 3 * PAGE]).unwrap();
  vol.snapshot(&mut store).unwrap();
  (store, vol, dir, f)
}

/// AC-1.4: write amplification per mutation is bounded by the depth in directory nodes: a
/// create five levels down after a snapshot copies exactly five directory nodes, and a second
/// create in the same epoch copies none.
#[test]
fn a_create_after_a_snapshot_copies_exactly_the_path_nodes() {
  let (mut store, mut vol, dir, _) = deep_tree_snapshotted();
  let nodes_before = store.dirs.iter().count();
  vol.create_file(&mut store, dir, "new", 0o644).unwrap();
  assert_eq!(
    store.dirs.iter().count() - nodes_before,
    5,
    "root, a, b, c, d copied once; nothing else"
  );
  vol.create_file(&mut store, dir, "again", 0o644).unwrap();
  assert_eq!(
    store.dirs.iter().count() - nodes_before,
    5,
    "the second create in the same epoch copies nothing"
  );
}

/// AC-1.4: write amplification per mutation is bounded by one extent of content: a one-byte
/// write into a three-page extent after a snapshot copies that extent (the buddy arena rounds
/// it to the next power-of-two pages, four), never the whole file; a write into a fresh window
/// takes one page. The snapshot's version is sealed by the copy-up, which trims its four-page block
/// to the three pages it holds (A-118), so the arena grows by the copy less that page.
#[test]
fn a_write_after_a_snapshot_copies_one_extent_or_takes_one_page() {
  let (mut store, mut vol, _, f) = deep_tree_snapshotted();
  let content_before = store.content.allocated_bytes();
  let trimmed_before = store.content.trimmed_bytes();
  vol.write(&mut store, f, 0, b"!").unwrap();
  let trimmed = usize::try_from(store.content.trimmed_bytes() - trimmed_before).unwrap();
  assert_eq!(
    trimmed, PAGE,
    "the sealed version gave back the page its block did not use"
  );
  assert_eq!(
    store.content.allocated_bytes() - content_before + trimmed,
    (3 * PAGE).next_power_of_two(),
    "the window's extent is copied whole, in the buddy's power-of-two pages"
  );
  let far = u64::try_from(store.content.chunk_bytes()).unwrap() * 4;
  let content_before = store.content.allocated_bytes();
  let trimmed_before = store.content.trimmed_bytes();
  vol.write(&mut store, f, far, b"!").unwrap();
  // Leaving the first window seals it, which trims its block to the three pages it holds (A-118).
  let trimmed = usize::try_from(store.content.trimmed_bytes() - trimmed_before).unwrap();
  assert_eq!(
    store.content.allocated_bytes() + trimmed - content_before,
    PAGE,
    "a fresh window takes one page"
  );
}

/// AC-1.4 (found 2026-10-09 profiling a 512 MiB overwrite over the macOS NFS mount: the busy shard spent 228 of its
/// samples in 2 s reading back, and mostly decrypting, the windows each write replaced). Do: write three windows of a
/// file, snapshot, overwrite the first two whole in one write from the file's start, then the third window's first
/// half. Expect: the two covered windows open without reading their sealed extents and the half-covered one is read
/// (the counter moves by exactly two); the head reads the new bytes and the third window's untouched tail; the
/// snapshot still reads every old byte.
#[test]
fn a_write_covering_a_sealed_window_whole_replaces_it_unread_and_a_partial_one_keeps_its_tail() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 26);
  let root = vol.root();
  let f = vol.create_file(&mut store, root, "f", 0o644).unwrap();
  let chunk = store.content.chunk_bytes();
  let old: Vec<u8> = (0..3 * chunk)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  vol.write(&mut store, f, 0, &old).unwrap();
  let s = vol.snapshot(&mut store).unwrap();
  let replaced_before = store.content.replaced_unread();
  let covering = vec![0xA5u8; 2 * chunk];
  vol.write(&mut store, f, 0, &covering).unwrap();
  let half = vec![0x5Au8; chunk / 2];
  let third = u64::try_from(2 * chunk).unwrap();
  vol.write(&mut store, f, third, &half).unwrap();
  assert_eq!(
    store.content.replaced_unread() - replaced_before,
    2,
    "the two covered windows were replaced unread; the half-covered one was read"
  );
  let mut expected = covering.clone();
  expected.extend_from_slice(&half);
  expected.extend_from_slice(&old[2 * chunk + chunk / 2..]);
  let mut head = vec![0u8; 3 * chunk];
  let read = vol.read(&store, f, 0, &mut head).unwrap();
  assert_eq!(read, 3 * chunk);
  assert!(
    head == expected,
    "the head reads the new bytes and the third window's old tail"
  );
  let mut snapshot = vec![0u8; 3 * chunk];
  let read = vol.read_in(&store, s, f, 0, &mut snapshot).unwrap();
  assert_eq!(read, 3 * chunk);
  assert!(snapshot == old, "the snapshot still reads every old byte");
}

/// AC-1.6: inode numbers are never reused within a volume and survive snapshot and clone.
#[test]
fn inode_numbers_are_never_reused_and_survive_snapshot_and_clone() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root();
  let mut last = 0u64;
  for round in 0..200 {
    let no = vol.create_file(&mut store, root, "f", 0o644).unwrap();
    assert!(
      no.counter() > last,
      "round {round}: {} after {last}",
      no.counter()
    );
    last = no.counter();
    vol.unlink(&mut store, root, "f").unwrap();
  }
  let kept = vol.create_file(&mut store, root, "kept", 0o644).unwrap();
  let s = vol.snapshot(&mut store).unwrap();
  assert_eq!(vol.resolve(&store, "/kept").unwrap().inode, kept);
  let clone = Volume::clone_of(&store, &mut vol, s, common::clone_config(8)).unwrap();
  assert_eq!(clone.resolve(&store, "/kept").unwrap().inode, kept);
  let next = vol.create_file(&mut store, root, "next", 0o644).unwrap();
  assert!(next.counter() > kept.counter());
}

/// POSIX `unlink()`: "if the file's link count is not 0, the last file status change timestamp of
/// the file shall be marked for update" (POSIX.1-2024 XSH); the same inode change happens when a
/// rename replaces one name of a file that keeps another. The link count is the inode's, so its
/// ctime moves with it — as it already did when a link was *added*. Found by pjdfstest `unlink/00.t`
/// and `rename/23.t` on 2026-09-15 (docs/bugs/2026-09-15-dropping-a-link-leaves-the-inodes-ctime.md).
#[test]
fn dropping_one_name_of_a_linked_file_advances_the_survivors_ctime() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root();
  let file = vol.create_file(&mut store, root, "f", 0o644).unwrap();
  vol.link(&mut store, root, "g", file).unwrap();
  let before = vol.stat(&store, file).unwrap();
  assert_eq!(before.nlink, 2);
  vol.unlink(&mut store, root, "g").unwrap();
  let after = vol.stat(&store, file).unwrap();
  assert_eq!(after.nlink, 1, "one name is gone");
  assert!(
    after.ctime > before.ctime,
    "unlinking one name marks the survivor's ctime: {} then {}",
    before.ctime,
    after.ctime
  );

  let target = vol.create_file(&mut store, root, "a", 0o644).unwrap();
  vol.link(&mut store, root, "b", target).unwrap();
  let before = vol.stat(&store, target).unwrap();
  assert_eq!(before.nlink, 2);
  vol.rename(&mut store, root, "f", root, "a").unwrap();
  let after = vol.stat(&store, target).unwrap();
  assert_eq!(after.nlink, 1, "the replaced name is gone, `b` remains");
  assert!(
    after.ctime > before.ctime,
    "a rename over one name marks the survivor's ctime: {} then {}",
    before.ctime,
    after.ctime
  );
  assert_eq!(
    vol.resolve(&store, "/a").unwrap().inode,
    file,
    "the renamed file took the name"
  );
}

/// CWE-190 (integer overflow) at the write/read offset. A client's write offset plus its length must be checked,
/// not wrapped: `off.checked_add(len)` refuses `FileTooLarge` rather than wrapping to a low offset and writing
/// out of place, and a read at a colossal offset returns no bytes rather than underflowing `size - off`. Do:
/// write four bytes at `u64::MAX - 2` and at `u64::MAX` (both overflow the end), read at `u64::MAX`. Expect:
/// each write refused `FileTooLarge`, the file still empty (no wrapped write), the read returns zero bytes, and
/// a normal write at offset 0 still works.
#[test]
fn a_write_or_read_at_a_colossal_offset_is_refused_or_empty_never_wrapped() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();

  for off in [u64::MAX - 2, u64::MAX] {
    assert_eq!(
      vol.write(&mut store, f, off, b"data"),
      Err(VfsError::FileTooLarge),
      "a write whose offset {off} plus length overflows must be refused, not wrapped"
    );
  }
  assert_eq!(
    vol.stat(&store, f).unwrap().size,
    0,
    "the overflowing writes wrote nothing — no wraparound to a low offset"
  );

  let mut buf = vec![0u8; 16];
  assert_eq!(
    vol.read(&store, f, u64::MAX, &mut buf).unwrap(),
    0,
    "a read at a colossal offset returns nothing, never an out-of-bounds or underflowing read"
  );

  vol.write(&mut store, f, 0, b"ok").unwrap();
  let mut got = vec![0u8; 2];
  assert_eq!(vol.read(&store, f, 0, &mut got).unwrap(), 2);
  assert_eq!(&got, b"ok", "a normal write reads back");
}

/// MEMORY-EXHAUSTION ATTACK → FIX (CLAUDE.md #8 no unbounded growth; CWE-789). A symlink's target is stored
/// inline in its inode and, before the fix, was neither length-capped nor byte-charged: an agent created
/// symlinks with wire-cap targets and committed target memory far past its quota (a real run put 81.9 MB of
/// targets behind a 64 KiB quota). A target is a path, so it is now capped at the host/archive path ceiling
/// (`PATH_MAX`), refused `NameTooLong` (`ENAMETOOLONG`) before any effect — a bound on each target, config-
/// independent, that a host `symlink(2)` also imposes. Do: create a symlink one byte past the ceiling, and one
/// exactly at it. Expect: the first refused `NameTooLong` with nothing created; the second lands and reads back.
#[test]
fn a_symlink_target_past_the_path_ceiling_is_refused() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root_inode(&store).unwrap();
  let ceiling = slates_archive::manifest::PATH_MAX_BYTES;

  let over = "x".repeat(ceiling + 1);
  assert_eq!(
    vol.symlink_no(&mut store, root, "toolong", &over),
    Err(VfsError::NameTooLong),
    "a symlink target past the path ceiling must be refused ENAMETOOLONG, not stored"
  );
  assert!(
    vol.lookup_no(&store, root, "toolong").is_err(),
    "nothing was created for the refused over-long target"
  );

  let at = "y".repeat(ceiling);
  let no = vol.symlink_no(&mut store, root, "ok", &at).unwrap();
  assert_eq!(
    vol.readlink(&store, no).unwrap().len(),
    ceiling,
    "a target exactly at the ceiling lands and reads back whole"
  );
}

/// A symlink target with an interior NUL byte is refused `InvalidName`, never stored (CWE-158; POSIX.1-2017
/// §3.271 — a pathname is a NUL-terminated string, and `symlink(2)` returns `EINVAL`). Without this, the stored
/// target diverges from what resolves: a FUSE readlink is NUL-terminated and truncates `"a\0b"` to `"a"` at the
/// kernel, while an NFS (length-prefixed XDR) readlink returns the whole `"a\0b"`, and a host `symlink(2)` or the
/// placed archive would refuse or truncate it — a volume must not hold a target it cannot carry consistently. Do:
/// create a symlink whose target has an interior NUL, then (non-vacuity) a legitimate multi-component path target.
/// Expect: the first refused `InvalidName` with nothing created; the second (containing `/`, no NUL) lands and
/// reads back byte-for-byte, proving the check refuses only the NUL, not a path's separators.
#[test]
fn a_symlink_target_with_an_interior_nul_is_refused_but_a_path_is_kept() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root_inode(&store).unwrap();

  assert_eq!(
    vol.symlink_no(&mut store, root, "nul", "a\0b"),
    Err(VfsError::InvalidName),
    "a symlink target with an interior NUL must be refused, not stored with a byte that truncates on readlink"
  );
  assert!(
    vol.lookup_no(&store, root, "nul").is_err(),
    "nothing was created for the refused NUL target"
  );

  let path = "../some/deep/relative/target";
  let no = vol.symlink_no(&mut store, root, "rel", path).unwrap();
  assert_eq!(
    &*vol.readlink(&store, no).unwrap(),
    path,
    "a legitimate multi-component path target (slashes, no NUL) is unaffected and reads back whole"
  );
}

/// CVE-2026-47243 (Kata runtime-rs virtiofs guest→host, 2026): a guest-root submits a raw `FUSE_SYMLINK` whose
/// NAME is an absolute host path (`/etc/cron.d/evil`) to drop a root-owned symlink in a sensitive host directory.
/// Replicated here at the layer that defends it — a symlink NAME is a single path component, so `names::check`
/// refuses one holding a separator (`/`) with `InvalidName` and nothing is created; the CVE's primitive cannot
/// name a host path. The companion leg: a symlink whose valid name carries an absolute-path TARGET is stored (a
/// target is just inline bytes in the RAM volume, capped and NUL-free), and reaching the host from it is R1's
/// grant-gated landing — which writes only inside the human's named target via per-component `O_NOFOLLOW`
/// (`land/tests/os.rs`), never `/etc/cron.d`. Do: create a symlink whose name is an absolute host path, then one
/// whose target is. Expect: the first refused `InvalidName` with nothing created; the second stored in the volume
/// and read back verbatim (inert until a landing a human grants, which cannot step out of its target).
#[test]
fn a_symlink_cannot_name_an_absolute_host_path_cve_2026_47243() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root_inode(&store).unwrap();

  assert_eq!(
    vol.symlink_no(&mut store, root, "/etc/cron.d/evil", "payload"),
    Err(VfsError::InvalidName),
    "a symlink NAME that is an absolute host path must be refused — the CVE-2026-47243 primitive names a host path"
  );
  assert!(
    vol.lookup_no(&store, root, "/etc/cron.d/evil").is_err(),
    "nothing was created for the refused absolute-path name"
  );

  // A valid name with an absolute-path target: stored as inline RAM bytes (R1), inert until a granted landing.
  let target = "/etc/cron.d/x";
  let no = vol.symlink_no(&mut store, root, "evil", target).unwrap();
  assert_eq!(
    &*vol.readlink(&store, no).unwrap(),
    target,
    "an absolute-path target is a stored RAM string (no host write without a grant), read back verbatim"
  );
}

/// A symlink inode answers only `readlink` — reading, writing, or seeking it as file content is refused `Invalid`
/// (POSIX `EINVAL`), like a directory and an IPC name beside it in the content paths. Found by attacking: before
/// this, `read` of a symlink's number returned its target bytes as if they were a file's content, and `write`
/// returned `Ok(n)` while silently dropping the bytes — the body is a target string, not a content body, so the
/// write was a no-op that still charged quota and lied about success. The kernel follows a symlink, but an SDK or
/// a crafted FUSE/NFS op can name its inode directly. Do: make a symlink; read, write, and seek it; then a regular
/// file. Expect: every symlink content op refused `Invalid`, the target unchanged and still readable through
/// `readlink`; the regular file's read and write still work (non-vacuity — the refusal is symlink-specific).
#[test]
fn a_symlink_answers_only_readlink_not_file_content() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root_inode(&store).unwrap();
  let link = vol.symlink_no(&mut store, root, "l", "the-target").unwrap();
  let mut buf = [0u8; 16];

  assert_eq!(
    vol.read(&store, link, 0, &mut buf),
    Err(VfsError::Invalid),
    "a read of a symlink's number must be EINVAL, never its target returned as file content"
  );
  assert_eq!(
    vol.write(&mut store, link, 0, b"overwrite"),
    Err(VfsError::Invalid),
    "a write of a symlink's number must be EINVAL, never a no-op that charges quota and lies Ok"
  );
  assert_eq!(
    vol.seek(&store, link, 0, slates_vfs::volume::Seek::Data),
    Err(VfsError::Invalid),
    "a seek of a symlink's number must be EINVAL"
  );
  assert_eq!(
    vol.truncate(&mut store, link, 100),
    Err(VfsError::Invalid),
    "a truncate of a symlink's number must be EINVAL, never a no-op that lies Ok"
  );
  assert_eq!(
    vol.admit_allocation(&mut store, link, 0, 100),
    Err(VfsError::Invalid),
    "an allocation of a symlink's number must be EINVAL (the match's _ => Ok formerly admitted it)"
  );
  assert_eq!(
    vol.edit(&mut store, link, 0, 0, b"x"),
    Err(VfsError::Invalid),
    "an SDK edit of a symlink's number must be EINVAL, refused at the kind check not a later offset check"
  );
  assert_eq!(
    &*vol.readlink(&store, link).unwrap(),
    "the-target",
    "the target is unchanged and still reachable through readlink, the symlink's one read"
  );

  // Non-vacuity: a regular file still reads and writes — the refusal is symlink-specific, not a blanket break.
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  assert_eq!(vol.write(&mut store, file, 0, b"hello").unwrap(), 5);
  assert_eq!(vol.read(&store, file, 0, &mut buf).unwrap(), 5);
}
