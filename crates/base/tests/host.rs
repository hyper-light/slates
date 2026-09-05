//! The operating-system host against the workspace's own tree (read-only, always on) and
//! against a RAM-backed directory the environment names (writes by the test only, to inject
//! outsider edits; skipped loudly without `SLATES_TEST_RAMDIR`).

// Test harness code: an unwrap here is a failed test. The RAM-directory tests write there
// through the standard library to play the outsider; the crate under test writes nothing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]

use std::path::PathBuf;

use slates_base::OsHost;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::base::BaseConfig;
use slates_vfs::clock::StepClock;
use slates_vfs::host::{HostError, HostFs, HostKind, WatchState};
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Format: the page size the tests build stores with.
const PAGE: usize = 4096;

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

/// Over a RAM-backed directory: a held descriptor keeps serving a file replaced beneath it, a
/// symlink is never followed as a directory or a file, and the watcher hints at a change.
#[cfg(unix)]
#[test]
fn a_ram_directory_shows_descriptor_semantics_nofollow_and_hints() {
  let Some(base) = std::env::var_os("SLATES_TEST_RAMDIR") else {
    println!("host: skipped — SLATES_TEST_RAMDIR is not set (name a RAM-backed directory)");
    return;
  };
  let dir = PathBuf::from(base).join(format!("slates-host-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir(&dir).unwrap();
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
  std::fs::remove_dir_all(&dir).unwrap();
}
