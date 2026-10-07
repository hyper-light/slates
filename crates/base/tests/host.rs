//! The operating-system host against the workspace's own tree (read-only, always on) and
//! against a real directory in the build output (`CARGO_TARGET_TMPDIR`, under `target/`; A-50:
//! never `/tmp`, never a RAM directory), written by the test only, to inject outsider edits.

// Test harness code: an unwrap here is a failed test. The build-output tests write there
// through the standard library to play the outsider; the crate under test writes nothing.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::disallowed_methods,
  clippy::indexing_slicing
)]

use std::path::PathBuf;

use slates_base::OsHost;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::base::{BaseConfig, DigestStats};
use slates_vfs::clock::StepClock;
use slates_vfs::error::VfsError;
use slates_vfs::host::{HostDir, HostError, HostFacts, HostFs, HostKind, WatchState};
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Format: the page size the tests build stores with.
const PAGE: usize = 4096;

/// An overlay volume over an opened host root, the fixture of the digest differentials.
fn overlay_over(store: &mut Store, root: HostDir, facts: HostFacts) -> Volume {
  Volume::create_overlay(
    store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(1_000_000_000, 1_000)),
    },
    BaseConfig {
      root,
      facts,
      large_class_bytes: 1 << 16,
    },
  )
  .unwrap()
}

fn digest_stats(vol: &Volume) -> DigestStats {
  vol.base_plane().unwrap().digest_stats()
}

fn crates_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .unwrap()
    .to_path_buf()
}

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * 4096, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 64,
      max_dirs: 1 << 16,
      max_inodes: 1 << 16,
      max_chunks: 1 << 16,
      max_dir_blocks: 1 << 16,
      dir_cutover: 2,
    },
    arena,
    0,
  )
}

/// Listings, opens, fingerprints and reads over the workspace's `crates/` directory agree
/// with the standard library's view of the same files.
#[test]
fn the_host_lists_opens_and_reads_the_workspace_tree() {
  let (mut host, root) = OsHost::open_root(&crates_dir()).unwrap();
  let facts = host.facts(root).unwrap();
  assert!(facts.timestamp_granularity_ns >= 1);
  let listing = host.list(root).unwrap();
  let vfs = listing
    .iter()
    .find(|e| e.name.as_ref() == "vfs")
    .expect("crates/vfs listed");
  assert_eq!(vfs.kind, HostKind::Dir);
  assert!(listing.iter().any(|e| e.name.as_ref() == "base"));

  let vfs_dir = host.open_dir(root, "vfs").unwrap();
  let inner = host.list(vfs_dir).unwrap();
  let manifest = inner
    .iter()
    .find(|e| e.name.as_ref() == "Cargo.toml")
    .expect("Cargo.toml listed");
  assert_eq!(manifest.kind, HostKind::File);
  let expected = std::fs::read(crates_dir().join("vfs/Cargo.toml")).unwrap();
  assert_eq!(
    manifest.fingerprint.size,
    u64::try_from(expected.len()).unwrap()
  );
  let file = host.open_file(vfs_dir, "Cargo.toml").unwrap();
  let fp = host.fstat(file).unwrap();
  assert_eq!(
    fp, manifest.fingerprint,
    "the listing's fingerprint is the file's"
  );
  assert_eq!(read_whole(&mut host, file, expected.len() + 16), expected);
  // The host's clock is in its fingerprints' domain (the racy rule's "now"): a file written in
  // the past has a modification time no later than now.
  assert!(
    host.now_ns() >= fp.mtime_ns,
    "now ({}) precedes the manifest's mtime ({}): the clocks are in different domains",
    host.now_ns(),
    fp.mtime_ns
  );
}

fn read_whole(host: &mut OsHost, file: slates_vfs::host::HostFile, cap: usize) -> Vec<u8> {
  let mut buf = vec![0u8; cap];
  let mut done = 0;
  loop {
    let n = host
      .read_at(file, u64::try_from(done).unwrap(), &mut buf[done..])
      .unwrap();
    if n == 0 {
      break;
    }
    done += n;
  }
  buf.truncate(done);
  buf
}

/// Opens never cross kinds (a directory is not a file, a file is not a directory, a missing
/// name is not found), and closed handles are gone.
#[test]
fn the_host_refuses_the_wrong_kind_and_releases_handles() {
  let (mut host, root) = OsHost::open_root(&crates_dir()).unwrap();
  let vfs_dir = host.open_dir(root, "vfs").unwrap();
  let file = host.open_file(vfs_dir, "Cargo.toml").unwrap();
  assert_eq!(
    host.open_file(root, "vfs"),
    Err(HostError::NotFile),
    "a directory is not a file"
  );
  assert_eq!(
    host.open_dir(vfs_dir, "Cargo.toml"),
    Err(HostError::NotDirectory)
  );
  assert_eq!(
    host.open_file(vfs_dir, "no-such-file"),
    Err(HostError::NotFound)
  );
  assert!(host.read_link(vfs_dir, "Cargo.toml").is_err());
  assert_eq!(host.open_handles(), 3);
  host.close_file(file);
  host.close_dir(vfs_dir);
  assert_eq!(host.open_handles(), 1);
}

/// AUD-29-62 (containment, both hosts). Do: open `crates/vfs` as the base, then ask it for its parent, for
/// itself, and for paths through it — `..`, `.`, `src/lib.rs`, `../base` — as directories, files and links.
/// Expect: each is refused as absent and no handle is kept; a lookup names one entry of its directory,
/// never a path. Before, the Unix host's `openat(dir, "..", O_NOFOLLOW)` opened the base's parent: the
/// flag guards a final symlink, never `..` or a separator.
#[test]
fn a_lookup_that_is_not_one_entry_never_leaves_the_base() {
  let (mut host, root) = OsHost::open_root(&crates_dir().join("vfs")).unwrap();
  for name in ["..", ".", "", "src/lib.rs", "../base", "src/../../base"] {
    assert_eq!(
      host.open_dir(root, name),
      Err(HostError::NotFound),
      "dir {name:?}"
    );
    assert_eq!(
      host.open_file(root, name),
      Err(HostError::NotFound),
      "file {name:?}"
    );
    assert_eq!(
      host.read_link(root, name),
      Err(HostError::NotFound),
      "link {name:?}"
    );
  }
  assert_eq!(host.open_handles(), 1, "no refused lookup kept a handle");
  host.close_dir(root);
}

/// AC-1.9 on a real tree: an overlay over `crates/` costs one directory open and one node; a
/// path resolution opens only the directories on it, and a read returns the disk's bytes.
#[test]
fn an_overlay_over_the_workspace_tree_costs_one_open_and_reads_the_disk() {
  let (mut host, root) = OsHost::open_root(&crates_dir()).unwrap();
  let facts = host.facts(root).unwrap();
  let mut store = store();
  let mut vol = Volume::create_overlay(
    &mut store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(1_000_000_000, 1_000)),
    },
    BaseConfig {
      root,
      facts,
      large_class_bytes: 1 << 16,
    },
  )
  .unwrap();
  assert_eq!(host.open_handles(), 1);
  assert_eq!(store.dirs.iter().count(), 1);
  let expected = std::fs::read(crates_dir().join("vfs/src/lib.rs")).unwrap();
  let mut o = vol.with_host(&mut host);
  let no = o.resolve(&mut store, "/vfs/src/lib.rs").unwrap().inode;
  let size = o.stat(&mut store, no).unwrap().size;
  assert_eq!(size, u64::try_from(expected.len()).unwrap());
  let mut buf = vec![0u8; expected.len()];
  let n = o.read(&mut store, no, 0, &mut buf).unwrap();
  assert_eq!(&buf[..n], &expected[..]);
  assert_eq!(
    host.open_handles(),
    4,
    "root, vfs, src, and the file's descriptor"
  );
  assert!(vol.diverged(&store).is_empty());
  assert_eq!(
    vol.with_host(&mut host).status(&mut store).unwrap().watcher,
    if cfg!(windows) {
      WatchState::Unavailable
    } else {
      WatchState::Live
    }
  );
}

/// §4.15 on a real tree, the differential against the host filesystem (always on, read only): the
/// digest of an untouched file under an overlay over `crates/` is the BLAKE3 of the bytes the
/// standard library reads from the same file, with its length; a directory refuses as itself.
#[test]
fn an_overlay_over_the_workspace_tree_digests_a_file_as_the_disk_holds_it() {
  let (mut host, root) = OsHost::open_root(&crates_dir()).unwrap();
  let facts = host.facts(root).unwrap();
  let mut store = store();
  let mut vol = overlay_over(&mut store, root, facts);
  let expected = std::fs::read(crates_dir().join("vfs/Cargo.toml")).unwrap();
  let digest = vol
    .with_host(&mut host)
    .digest(&mut store, "/vfs/Cargo.toml")
    .unwrap();
  assert_eq!(digest.identity, *blake3::hash(&expected).as_bytes());
  assert_eq!(digest.size, u64::try_from(expected.len()).unwrap());
  assert_eq!(
    vol.with_host(&mut host).digest(&mut store, "/vfs"),
    Err(VfsError::IsDirectory)
  );
  assert_eq!(digest_stats(&vol).computed, 1);
}

/// Shape: the pause that lets a filesystem's timestamp tick close so a digest may be kept
/// (nanosecond granularity on tmpfs and APFS, a microsecond clock resolution on macOS): two
/// milliseconds, far past both.
#[cfg(unix)]
const TICK: std::time::Duration = std::time::Duration::from_millis(2);

/// A test's own directory in the build output (`CARGO_TARGET_TMPDIR`; A-50: a real base directory,
/// never `/tmp` or a RAM directory), named with the process id and removed on drop, a failed assertion
/// included.
struct BuildOutputDir(PathBuf);

impl BuildOutputDir {
  fn new(slug: &str) -> Self {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
      .join(format!("slates-{slug}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    Self(dir)
  }
}

impl Drop for BuildOutputDir {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.0);
  }
}

/// Over a real directory in the build output, the watcher leg of
/// §4.15 on a real filesystem: a kept digest follows the disk through the real watcher — a
/// replacement by rename hints the directory, the hint's revalidation drops the kept digest, and
/// the next export names the new bytes; a later change elsewhere in the directory hints again,
/// re-verifies the kept digest and keeps it, and the export after it is reused.
#[cfg(unix)]
#[test]
fn a_real_directory_digest_follows_the_disk_through_the_watcher() {
  // A-50: a real base directory in the build output, never `/tmp` or a RAM directory.
  let owned = BuildOutputDir::new("digest");
  let dir = owned.0.clone();
  std::fs::write(dir.join("f"), b"one").unwrap();
  std::thread::sleep(TICK);

  let (mut host, root) = OsHost::open_root(&dir).unwrap();
  let facts = host.facts(root).unwrap();
  let mut store = store();
  let mut vol = overlay_over(&mut store, root, facts);
  let first = vol.with_host(&mut host).digest(&mut store, "/f").unwrap();
  assert_eq!(first.identity, *blake3::hash(b"one").as_bytes());
  assert_eq!(digest_stats(&vol).cached, 1, "the tick had closed: kept");
  let rechecks = assert_replacement_hint_drops_the_digest(&mut vol, &mut host, &mut store, &dir);
  assert_unrelated_change_hint_keeps_the_digest(&mut vol, &mut host, &mut store, &dir, rechecks);
}

/// Replace `f` by rename (a new inode at the name): the real watcher hints the directory, the
/// hint's revalidation drops the kept digest, and the next export names the new bytes. Returns
/// the hint re-checks so far.
#[cfg(unix)]
fn assert_replacement_hint_drops_the_digest(
  vol: &mut Volume,
  host: &mut OsHost,
  store: &mut Store,
  dir: &std::path::Path,
) -> u64 {
  std::fs::write(dir.join("f.tmp"), b"two").unwrap();
  std::fs::rename(dir.join("f.tmp"), dir.join("f")).unwrap();
  vol.with_host(host).status(store).unwrap();
  let after_hint = digest_stats(vol);
  assert!(
    after_hint.hint_rechecked >= 1,
    "the hint re-verified the kept digest: {after_hint:?}"
  );
  assert_eq!(
    after_hint.stale, 1,
    "the replaced file's digest was dropped"
  );
  assert_eq!(after_hint.cached, 0);
  let second = vol.with_host(host).digest(store, "/f").unwrap();
  assert_eq!(second.identity, *blake3::hash(b"two").as_bytes());
  after_hint.hint_rechecked
}

/// The tick closes and the digest is kept; a change elsewhere in the directory hints, the kept
/// digest is re-verified and survives, and the next export reuses it.
#[cfg(unix)]
fn assert_unrelated_change_hint_keeps_the_digest(
  vol: &mut Volume,
  host: &mut OsHost,
  store: &mut Store,
  dir: &std::path::Path,
  rechecks_before: u64,
) {
  std::thread::sleep(TICK);
  vol.with_host(host).digest(store, "/f").unwrap();
  assert_eq!(digest_stats(vol).cached, 1);
  std::fs::write(dir.join("g"), b"other").unwrap();
  vol.with_host(host).status(store).unwrap();
  let rechecked = digest_stats(vol);
  assert!(rechecked.hint_rechecked > rechecks_before, "{rechecked:?}");
  assert_eq!(
    rechecked.stale, 1,
    "nothing beneath the kept digest changed"
  );
  assert_eq!(rechecked.cached, 1);
  let reused = vol.with_host(host).digest(store, "/f").unwrap();
  assert_eq!(reused.identity, *blake3::hash(b"two").as_bytes());
  assert_eq!(reused.size, 3);
  let after = digest_stats(vol);
  assert_eq!(
    after.revalidated,
    rechecked.revalidated + 1,
    "reused after the hint"
  );
  assert_eq!(
    after.computed, rechecked.computed,
    "reuse did not hash again"
  );
}

/// Over a real directory in the build output: a held descriptor keeps serving a file replaced beneath it, a
/// symlink is never followed as a directory or a file, and the watcher hints at a change.
#[cfg(unix)]
#[test]
fn a_real_directory_shows_descriptor_semantics_nofollow_and_hints() {
  // A-50: a real base directory in the build output, never `/tmp` or a RAM directory.
  let owned = BuildOutputDir::new("host");
  let dir = owned.0.clone();
  std::fs::write(dir.join("f"), b"one").unwrap();
  std::fs::create_dir(dir.join("sub")).unwrap();
  std::os::unix::fs::symlink("sub", dir.join("link")).unwrap();

  let (mut host, root) = OsHost::open_root(&dir).unwrap();
  assert_eq!(host.watch(root), WatchState::Live);
  let file = host.open_file(root, "f").unwrap();
  let before = host.fstat(file).unwrap();
  // Replace by rename: a new inode at the name; the descriptor keeps the old one alive.
  std::fs::write(dir.join("f.tmp"), b"two").unwrap();
  std::fs::rename(dir.join("f.tmp"), dir.join("f")).unwrap();
  assert_eq!(host.fstat(file).unwrap().ino, before.ino);
  let mut buf = [0u8; 8];
  assert_eq!(host.read_at(file, 0, &mut buf).unwrap(), 3);
  assert_eq!(&buf[..3], b"one");
  let fresh = host.open_file(root, "f").unwrap();
  assert_ne!(host.fstat(fresh).unwrap().ino, before.ino);
  // The symlink is neither a directory nor a file to open through.
  assert_eq!(host.open_dir(root, "link"), Err(HostError::NotDirectory));
  assert_eq!(host.open_file(root, "link"), Err(HostError::NotFile));
  assert_eq!(host.read_link(root, "link").unwrap().as_ref(), "sub");
  let hints = host.hints();
  assert!(
    hints
      .iter()
      .any(|h| matches!(h, slates_vfs::host::Hint::Changed(d) if *d == root)),
    "a hint for the root after the rename: {hints:?}"
  );
  host.close_file(file);
  host.close_file(fresh);
  host.close_dir(root);
}

/// A junction at `link` to `target`, as a user makes one (`mklink /J`, no privilege needed).
#[cfg(windows)]
fn junction(link: &std::path::Path, target: &std::path::Path) {
  let made = std::process::Command::new("cmd")
    .args(["/C", "mklink", "/J"])
    .arg(link)
    .arg(target)
    .output()
    .unwrap();
  assert!(made.status.success(), "mklink /J: {made:?}");
}

/// A link at `link` to the directory `target`, as an outsider makes one without privilege: a junction on Windows, a
/// symbolic link on Unix.
#[cfg(windows)]
fn link_dir(link: &std::path::Path, target: &std::path::Path) {
  junction(link, target);
}

/// A link at `link` to the directory `target`, as an outsider makes one without privilege: a junction on Windows, a
/// symbolic link on Unix.
#[cfg(unix)]
fn link_dir(link: &std::path::Path, target: &std::path::Path) {
  std::os::unix::fs::symlink(target, link).unwrap();
}

/// A symbolic link at `link` to the file `target` (on Windows it needs a privilege or developer mode).
#[cfg(windows)]
fn link_file(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
  std::os::windows::fs::symlink_file(target, link)
}

/// A symbolic link at `link` to the file `target`.
#[cfg(unix)]
fn link_file(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
  std::os::unix::fs::symlink(target, link)
}

/// The bytes of the file `name` under `dir`, through the host.
#[cfg(any(unix, windows))]
fn read_named(host: &mut OsHost, dir: HostDir, name: &str) -> Vec<u8> {
  let file = host.open_file(dir, name).unwrap();
  let bytes = read_whole(host, file, 64);
  host.close_file(file);
  bytes
}

/// The names a listing shows, sorted.
#[cfg(any(unix, windows))]
fn listed(host: &mut OsHost, dir: HostDir) -> Vec<String> {
  let mut names: Vec<String> = host
    .list(dir)
    .unwrap()
    .into_iter()
    .map(|entry| entry.name.to_string())
    .collect();
  names.sort();
  names
}

/// Shape: the bytes every file outside the base holds; no host call may ever return them.
#[cfg(any(unix, windows))]
const SENTINEL: &[u8] = b"outside the base";
/// Shape: the bytes every file inside the base holds.
#[cfg(any(unix, windows))]
const INSIDE: &[u8] = b"inside the base";

/// A base (`base/f.txt`, `base/sub/inner.txt`) and a sibling outside it (`outside/sentinel.txt`,
/// `outside/f.txt`, `outside/inner.txt`) in a fresh build-output directory.
#[cfg(any(unix, windows))]
fn base_and_outside(slug: &str) -> (BuildOutputDir, PathBuf, PathBuf) {
  let owned = BuildOutputDir::new(slug);
  let base = owned.0.join("base");
  let outside = owned.0.join("outside");
  std::fs::create_dir_all(base.join("sub")).unwrap();
  std::fs::create_dir_all(&outside).unwrap();
  std::fs::write(base.join("f.txt"), INSIDE).unwrap();
  std::fs::write(base.join("sub").join("inner.txt"), INSIDE).unwrap();
  for name in ["sentinel.txt", "f.txt", "inner.txt"] {
    std::fs::write(outside.join(name), SENTINEL).unwrap();
  }
  (owned, base, outside)
}

/// AUD-29-62 (root swapped; junction on Windows, symbolic link on Unix since 2026-10-06). Do: open a base, then move the base away and put a junction to a directory
/// outside it at the base's path; list the root, read a file through it, open its subdirectory. Expect: the
/// retained root keeps naming the directory it opened — the moved base's entries and bytes, never the
/// outside directory's.
#[cfg(any(unix, windows))]
#[test]
fn a_retained_root_keeps_naming_the_base_after_its_path_becomes_a_junction() {
  let (owned, base, outside) = base_and_outside("contain-root");
  let (mut host, root) = OsHost::open_root(&base).unwrap();
  std::fs::rename(&base, owned.0.join("base-moved")).unwrap();
  link_dir(&base, &outside);
  assert_eq!(listed(&mut host, root), ["f.txt", "sub"]);
  assert_eq!(read_named(&mut host, root, "f.txt"), INSIDE);
  let sub = host.open_dir(root, "sub").unwrap();
  assert_eq!(read_named(&mut host, sub, "inner.txt"), INSIDE);
  host.close_dir(sub);
  host.close_dir(root);
  assert_eq!(host.open_handles(), 0);
}

/// AUD-29-62 (intermediate swapped). Do: open a base and its subdirectory, then move the subdirectory away
/// and put a junction to an outside directory at its name; list and read through the retained subdirectory,
/// and look the name up again from the root. Expect: the retained handle still names the moved directory;
/// the fresh lookup finds a link — refused as a directory and as a file, listed as a link, its target read
/// — and nothing returns the outside bytes. Before, each access re-resolved `base\sub` as a path and
/// followed the junction.
#[cfg(any(unix, windows))]
#[test]
fn an_intermediate_swapped_for_a_junction_is_never_traversed() {
  let (_owned, base, outside) = base_and_outside("contain-mid");
  let (mut host, root) = OsHost::open_root(&base).unwrap();
  let sub = host.open_dir(root, "sub").unwrap();
  std::fs::rename(base.join("sub"), base.join("sub-moved")).unwrap();
  link_dir(&base.join("sub"), &outside);
  assert_eq!(listed(&mut host, sub), ["inner.txt"]);
  assert_eq!(read_named(&mut host, sub, "inner.txt"), INSIDE);
  assert_eq!(host.open_dir(root, "sub"), Err(HostError::NotDirectory));
  assert_eq!(host.open_file(root, "sub"), Err(HostError::NotFile));
  let kinds: Vec<(String, HostKind)> = host
    .list(root)
    .unwrap()
    .into_iter()
    .map(|entry| (entry.name.to_string(), entry.kind))
    .collect();
  assert!(
    kinds.contains(&("sub".to_owned(), HostKind::Symlink)),
    "{kinds:?}"
  );
  let target = host.read_link(root, "sub").unwrap();
  assert!(
    target.ends_with("outside"),
    "the junction's target: {target}"
  );
  host.close_dir(sub);
  host.close_dir(root);
}

/// AUD-29-62 (final component swapped, and drift). Do: open a file in the base, then move it aside and put
/// a symbolic link to an outside file at its name; read through the handle opened before, and look the name
/// up again. Expect: the old handle keeps its object's bytes (the reviewed drift semantics: an open
/// authorized inode reads as it was); the fresh lookup is refused as a file and listed as a link. Creating a
/// file symbolic link needs a privilege or developer mode; without one the link half skips, saying so.
#[cfg(any(unix, windows))]
#[test]
fn a_final_component_swapped_for_a_link_is_refused_and_an_open_file_keeps_its_bytes() {
  let (_owned, base, outside) = base_and_outside("contain-final");
  let (mut host, root) = OsHost::open_root(&base).unwrap();
  let before = host.open_file(root, "f.txt").unwrap();
  std::fs::rename(base.join("f.txt"), base.join("f-moved.txt")).unwrap();
  if let Err(refused) = link_file(&outside.join("f.txt"), &base.join("f.txt")) {
    eprintln!("SKIP the link half: creating a file symbolic link was refused here ({refused})");
    assert_eq!(read_whole(&mut host, before, 64), INSIDE);
    return;
  }
  assert_eq!(read_whole(&mut host, before, 64), INSIDE);
  assert_eq!(host.open_file(root, "f.txt"), Err(HostError::NotFile));
  assert_eq!(host.open_dir(root, "f.txt"), Err(HostError::NotDirectory));
  let kinds: Vec<(String, HostKind)> = host
    .list(root)
    .unwrap()
    .into_iter()
    .map(|entry| (entry.name.to_string(), entry.kind))
    .collect();
  assert!(
    kinds.contains(&("f.txt".to_owned(), HostKind::Symlink)),
    "{kinds:?}"
  );
  assert!(host.read_link(root, "f.txt").unwrap().ends_with("f.txt"));
  host.close_file(before);
  host.close_dir(root);
}
