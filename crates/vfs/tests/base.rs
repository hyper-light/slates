//! The base plane's tests (Phase 1 task 10; §4.5, §4.15, D-25) over the simulated host: the
//! worked example of Phase 1, AC-1.9 (create costs one open, memory independent of the tree),
//! AC-1.10 (the overlay holds exactly the diverged entries, witnesses equal the disk at
//! copy-up), AC-1.11 (drift never absorbed; `BaseDrift` instead of torn bytes), T-1.11 (the racy
//! rule), T-1.12 (`rm -r` of a large base directory then two files inside), T-1.13 (watcher
//! overflow), and T-1.10's oracle over generated histories with outsider edits.

// Test harness code: an unwrap here is a failed test, which is what it should be. proptest's
// strategy types carry `Arc` (D-8's harness exception).
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::disallowed_types,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use slates_vfs::base::{BaseConfig, Divergence, DriftKind};
use slates_vfs::clock::StepClock;
use slates_vfs::dir::Child;
use slates_vfs::error::VfsError;
use slates_vfs::host::sim::SimHost;
use slates_vfs::host::{HostFs, HostKind, WatchState};
use slates_vfs::inode::{Fingerprint, Kind};
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, Volume, VolumeConfig};

mod common;
use common::store;

/// Shape: the large-file class boundary these tests use, one chunk window, so a file of two
/// windows is large class and pins one window at a time.
const LARGE: u64 = 65_536;

fn overlay(host: &mut SimHost, store: &mut Store) -> Volume {
  let root = host.root();
  let facts = host.facts(root).unwrap();
  Volume::create_overlay(
    store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(1_000_000, 1_000)),
    },
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap()
}

fn read_all(
  vol: &mut Volume,
  host: &mut SimHost,
  store: &mut Store,
  path: &str,
) -> Result<Vec<u8>, VfsError> {
  let mut o = vol.with_host(host);
  let no = o.resolve(store, path)?.inode;
  let size = o.stat(store, no)?.size;
  let mut buf = vec![0u8; usize::try_from(size).unwrap()];
  let n = o.read(store, no, 0, &mut buf)?;
  buf.truncate(n);
  Ok(buf)
}

/// A base tree of `files` files in `dirs` directories.
fn populate(host: &mut SimHost, dirs: usize, files: usize) {
  for d in 0..dirs {
    host.mkdir(&format!("/d{d}"));
  }
  for f in 0..files {
    let d = f % dirs.max(1);
    host.replace_file(&format!("/d{d}/f{f}"), format!("file {f}").as_bytes());
  }
}

/// AC-2.12 / T-2.14: retain an unvisited overlay, then read its source after recovery. Omitting the host refuses
/// instead of publishing an apparently empty scratch volume. A later recovery from the same image, after an outsider
/// added a file to the source, recovers too and shows the file: the source is the same directory, whose entries changed
/// (A-153; until 2026-10-22 this test asserted the refusal that lost such a volume).
#[test]
fn an_unvisited_overlay_recovers_its_verified_source() {
  let mut host = SimHost::new();
  host.replace_file("/unvisited", b"must not disappear");
  let mut store = store();
  let vol = overlay(&mut host, &mut store);
  assert_eq!(
    vol.to_image(&store, None),
    Err(VfsError::RecoveryIncomplete)
  );
  let image = vol.to_image(&store, Some(&mut host)).unwrap();
  let root = host.root();
  let mut after = common::surviving(&store);
  let claims = common::claims(&mut after, &[&image]);
  let mut recovered = Volume::from_image(
    &mut after,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 20,
    Some((&mut host, root)),
  )
  .unwrap();
  assert_eq!(
    read_all(&mut recovered, &mut host, &mut after, "/unvisited").unwrap(),
    b"must not disappear"
  );
  host.advance_ns(2);
  host.replace_file("/other", b"changed directory");
  let mut again = common::surviving(&store);
  let claims = common::claims(&mut again, &[&image]);
  let mut recovered_again = Volume::from_image(
    &mut again,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 20,
    Some((&mut host, root)),
  )
  .expect("a source whose entries changed is the same source");
  assert_eq!(
    read_all(&mut recovered_again, &mut host, &mut again, "/other").unwrap(),
    b"changed directory"
  );
  assert_eq!(
    read_all(&mut recovered_again, &mut host, &mut again, "/unvisited").unwrap(),
    b"must not disappear"
  );
}

/// A-153 on a filesystem that records no birth time: a directory's identity is then its device and inode number, so an
/// in-place change to its entries still recovers. Do: over such a disk, read a file so the root's listing is loaded,
/// image the overlay, add a file at the root as an outsider, close every descriptor and recover. Expect the recovery
/// to succeed and both files to read.
#[test]
fn without_birth_times_an_overlay_recovers_after_an_outsider_changed_its_base_directory() {
  let mut host = SimHost::new().without_birth_times();
  host.replace_file("/top.txt", b"at the root");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  read_all(&mut vol, &mut host, &mut store, "/top.txt").unwrap();
  let image = vol.to_image(&store, Some(&mut host)).unwrap();
  host.advance_ns(10);
  host.replace_file("/added.txt", b"added after the image");
  host.close_files();
  let root = host.root();
  let mut after = common::surviving(&store);
  let claims = common::claims(&mut after, &[&image]);
  let mut recovered = Volume::from_image(
    &mut after,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 20,
    Some((&mut host, root)),
  )
  .expect("without birth times, the device and inode number name the same directory");
  assert_eq!(
    read_all(&mut recovered, &mut host, &mut after, "/added.txt").unwrap(),
    b"added after the image"
  );
  assert_eq!(
    read_all(&mut recovered, &mut host, &mut after, "/top.txt").unwrap(),
    b"at the root"
  );
}

/// A takeover recovers an overlay whose base directories an outsider changed after its image (A-61, §4.5 recovery;
/// `docs/bugs/2026-10-22-a-takeover-refused-an-overlay-whose-base-directory-an-outsider-had-changed.md`). A directory
/// whose entries changed is still the directory the image recorded: recovery checks its identity, and its listing is
/// read again on first use, as every recovered listing is. Do: read a file in the root and one in a subdirectory, so
/// both listings are loaded, and image the overlay; then, as an outsider, replace the subdirectory's file, add a file
/// there and add one at the root; close every descriptor (the daemon's death) and recover. Expect the recovery to
/// succeed and every name to read the disk as it is now. Before the fix it refused `RecoveryIncomplete`, so a daemon
/// that died after any outsider edit in a base directory since its last publication lost the volume.
#[test]
fn an_overlay_recovers_after_an_outsider_changed_its_base_directories_since_its_image() {
  let mut host = SimHost::new();
  host.mkdir("/src");
  host.replace_file("/src/lib.rs", b"before the image");
  host.replace_file("/top.txt", b"at the root");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/src/lib.rs").unwrap(),
    b"before the image"
  );
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/top.txt").unwrap(),
    b"at the root"
  );
  let image = vol.to_image(&store, Some(&mut host)).unwrap();
  host.advance_ns(10);
  host.replace_file("/src/lib.rs", b"replaced after the image");
  host.replace_file("/src/new.rs", b"added after the image");
  host.replace_file("/added.txt", b"added at the root");
  host.close_files();
  let root = host.root();
  let mut after = common::surviving(&store);
  let claims = common::claims(&mut after, &[&image]);
  let mut recovered = Volume::from_image(
    &mut after,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 20,
    Some((&mut host, root)),
  )
  .expect("an overlay whose base directories changed in place recovers");
  for (path, bytes) in [
    ("/src/lib.rs", &b"replaced after the image"[..]),
    ("/src/new.rs", &b"added after the image"[..]),
    ("/added.txt", &b"added at the root"[..]),
    ("/top.txt", &b"at the root"[..]),
  ] {
    assert_eq!(
      read_all(&mut recovered, &mut host, &mut after, path).unwrap(),
      bytes,
      "{path} reads the disk as it is now"
    );
  }
}

/// The guard beside it: a base directory another directory replaced is not the directory the image recorded, and
/// reopening a path alone never substitutes another base (§4.5 recovery, "validated source identity"). Do: read a file
/// in a subdirectory and image the overlay; then make another directory and rename it over the subdirectory; recover.
/// Expect `RecoveryIncomplete`.
#[test]
fn an_overlay_whose_base_directory_another_replaced_is_refused() {
  let mut host = SimHost::new();
  host.mkdir("/src");
  host.replace_file("/src/lib.rs", b"the recorded tree");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  read_all(&mut vol, &mut host, &mut store, "/src/lib.rs").unwrap();
  let image = vol.to_image(&store, Some(&mut host)).unwrap();
  host.advance_ns(10);
  host.mkdir("/elsewhere");
  host.replace_file("/elsewhere/lib.rs", b"another tree");
  host.rename("/elsewhere", "/src");
  host.close_files();
  let root = host.root();
  let mut after = common::surviving(&store);
  let claims = common::claims(&mut after, &[&image]);
  assert!(matches!(
    Volume::from_image(
      &mut after,
      &image,
      &claims,
      Box::new(StepClock::new(0, 1)),
      1 << 20,
      Some((&mut host, root)),
    ),
    Err(VfsError::RecoveryIncomplete)
  ));
}

/// AC-2.12 / T-2.14, AC-1.11: recover redirected directories, whiteouts, a snapshot, metadata
/// witnesses and partial private extents. Changed unpinned source bytes refuse; pinned bytes survive.
#[test]
fn an_overlay_recovers_its_private_state_and_refuses_source_drift() {
  let mut host = SimHost::new();
  host.mkdir("/source");
  host.replace_file(
    "/source/large",
    &vec![b'a'; usize::try_from(LARGE * 3).unwrap()],
  );
  host.replace_file("/source/meta", b"metadata witness");
  host.replace_file("/source/gone", b"hidden");
  host.advance_ns(2);
  let mut before = store();
  let mut vol = overlay(&mut host, &mut before);
  let large = vol
    .with_host(&mut host)
    .resolve(&mut before, "/source/large")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .write(&mut before, large, 10, b"private")
    .unwrap();
  let meta = vol
    .with_host(&mut host)
    .resolve(&mut before, "/source/meta")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .chmod(&mut before, meta, 0o600)
    .unwrap();
  let Child::Dir(source) = vol
    .with_host(&mut host)
    .resolve(&mut before, "/source")
    .unwrap()
    .child
  else {
    panic!("directory");
  };
  vol
    .with_host(&mut host)
    .unlink(&mut before, source, "gone")
    .unwrap();
  let root = vol.root();
  vol
    .with_host(&mut host)
    .rename(&mut before, root, "source", root, "moved")
    .unwrap();
  let snapshot = vol.snapshot(&mut before).unwrap();
  let expected = read_all(&mut vol, &mut host, &mut before, "/moved/large").unwrap();
  let divergence = vol.diverged(&before);
  let image = vol.to_image(&before, Some(&mut host)).unwrap();
  let encoded = image.to_content();
  let image = slates_vfs::recover::VolumeImage::from_content(&encoded).unwrap();
  let mut after = common::surviving(&before);
  let root = host.root();
  let claims = common::claims(&mut after, &[&image]);
  let mut recovered = Volume::from_image(
    &mut after,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 20,
    Some((&mut host, root)),
  )
  .unwrap();
  assert_eq!(recovered.diverged(&after), divergence);
  assert_eq!(
    read_all(&mut recovered, &mut host, &mut after, "/moved/large").unwrap(),
    expected
  );
  assert_eq!(
    read_all(&mut recovered, &mut host, &mut after, "/moved/meta").unwrap(),
    b"metadata witness"
  );
  assert_eq!(recovered.stat(&after, meta).unwrap().mode & 0o777, 0o600);
  assert_eq!(
    read_all(&mut recovered, &mut host, &mut after, "/moved/gone"),
    Err(VfsError::NotFound)
  );
  assert_eq!(
    read_all(&mut recovered, &mut host, &mut after, "/source/large"),
    Err(VfsError::NotFound)
  );
  let mut frozen = [0; 7];
  assert_eq!(
    recovered
      .read_in(&after, snapshot, large, 10, &mut frozen)
      .unwrap(),
    frozen.len()
  );
  assert_eq!(&frozen, b"private");
  assert_recovered_drift(&mut recovered, &mut host, &mut after, large);
}

fn assert_recovered_drift(
  volume: &mut Volume,
  host: &mut SimHost,
  store: &mut Store,
  large: slates_vfs::ids::InodeNo,
) {
  host.write_in_place("/source/large", b"replaced in place", 5_000_000_000);
  let mut bytes = [0; 7];
  assert_eq!(
    volume
      .with_host(host)
      .read(store, large, 10, &mut bytes)
      .unwrap(),
    bytes.len()
  );
  assert_eq!(&bytes, b"private");
  assert_eq!(
    volume
      .with_host(host)
      .read(store, large, LARGE * 2, &mut bytes),
    Err(VfsError::BaseDrift)
  );
}

/// AC-1.9: create over a base directory costs one directory open regardless of tree size, and
/// memory after create is independent of the tree.
#[test]
fn create_over_a_base_costs_one_open_and_no_memory_whatever_the_tree_size() {
  for (dirs, files) in [(4usize, 1_000usize), (64, 100_000), (256, 1_000_000)] {
    let mut host = SimHost::new();
    populate(&mut host, dirs, files);
    let mut store = store();
    let vol = overlay(&mut host, &mut store);
    assert_eq!(
      host.open_handles(),
      1,
      "one directory open at {files} files"
    );
    assert_eq!(store.dirs.iter().count(), 1, "the root node only");
    assert_eq!(store.inodes.iter().count(), 1, "the root inode only");
    assert!(vol.diverged(&store).is_empty());
  }
}

/// The base of the worked example: `src/lib.rs` and `src/main.rs`, with `lib.rs` read once
/// and then edited by the agent (copied up). The files' timestamp tick closes before the volume
/// lists them, as on a real disk written before the agent starts, so the witness is not racy.
fn worked_example() -> (SimHost, Store, Volume, slates_vfs::ids::InodeNo) {
  let mut host = SimHost::new();
  host.mkdir("/src");
  host.replace_file("/src/lib.rs", b"pub fn lib() {}");
  host.replace_file("/src/main.rs", b"fn main() {}");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/src/lib.rs").unwrap(),
    b"pub fn lib() {}"
  );
  assert_eq!(
    host.open_handles(),
    3,
    "the root, src, and the file's descriptor"
  );
  let lib = vol
    .with_host(&mut host)
    .resolve(&mut store, "/src/lib.rs")
    .unwrap()
    .inode;
  assert!(vol.diverged(&store).is_empty(), "a lookup does not diverge");
  vol
    .with_host(&mut host)
    .write(&mut store, lib, 0, b"pub")
    .unwrap();
  (host, store, vol, lib)
}

/// The overlay worked example of §4.4 and Phase 1, first half: a lookup loads one listing; a
/// write copies up with a witness equal to the disk.
#[test]
fn the_overlay_worked_example_copies_up_with_a_witness_equal_to_the_disk() {
  let (mut host, mut store, mut vol, lib) = worked_example();
  let witness = vol
    .base_plane()
    .unwrap()
    .witness(lib)
    .expect("witnessed at copy-up");
  assert_eq!(
    witness.fingerprint,
    host.fingerprint("/src/lib.rs").unwrap()
  );
  assert_eq!(
    witness.identity,
    *blake3::hash(b"pub fn lib() {}").as_bytes()
  );
  assert!(!witness.racy);
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/src/lib.rs").unwrap(),
    b"pub fn lib() {}"
  );
  let diverged = vol.diverged(&store);
  assert_eq!(diverged.len(), 1);
  assert_eq!(diverged[0].path, "/src/lib.rs");
  assert_eq!(diverged[0].kind, Divergence::Witnessed);
}

/// A reclaimed base file's descriptor is closed (2026-10-07). Do: read `/src/main.rs` through the overlay (its
/// descriptor is held, so the host holds the root, `src` and the file open), unlink it through the overlay (no
/// reference holds it, so it is reclaimed at once), then drain the host's hints. Expect: the host holds the two
/// directories and nothing else. Before, the reclaim took the descriptor out of the plane's table and dropped it
/// without closing it: on a real disk, a file descriptor leaked per unlinked base file.
#[test]
fn an_unlinked_base_files_descriptor_is_closed() {
  let mut host = SimHost::new();
  host.mkdir("/src");
  host.replace_file("/src/main.rs", b"fn main() {}");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/src/main.rs").unwrap(),
    b"fn main() {}"
  );
  assert_eq!(
    host.open_handles(),
    3,
    "the root, src, and the file's descriptor"
  );
  let src = match vol
    .with_host(&mut host)
    .resolve(&mut store, "/src")
    .unwrap()
    .child
  {
    Child::Dir(handle) => handle,
    other => panic!("{other:?}"),
  };
  vol
    .with_host(&mut host)
    .unlink(&mut store, src, "main.rs")
    .unwrap();
  vol.with_host(&mut host).process_hints(&mut store).unwrap();
  assert_eq!(
    host.open_handles(),
    2,
    "the root and src, the file's descriptor closed"
  );
}

/// A drift re-check the host refuses is reported, never passed (2026-10-07). Do: in the worked example, arm the host
/// to refuse opening the witnessed `/src/lib.rs` (an I/O error) and ask for the status; then clear the fault and ask
/// again. Expect: the first status lists `/src/lib.rs` as unverified with the host's refusal and no drift; the
/// second lists nothing unverified. Before, the refusal was dropped and the entry looked as if it matched its witness.
#[test]
fn a_drift_recheck_the_host_refuses_is_reported_unverified() {
  let (mut host, mut store, mut vol, _) = worked_example();
  host.fail(
    slates_vfs::host::sim::SimVerb::OpenFile,
    "/src/lib.rs",
    slates_vfs::host::HostError::Unavailable(5),
    u32::MAX,
  );
  let status = vol.with_host(&mut host).status(&mut store).unwrap();
  assert!(status.drift.is_empty(), "{:?}", status.drift);
  assert_eq!(
    status
      .unverified
      .iter()
      .map(|(path, _)| path.as_str())
      .collect::<Vec<_>>(),
    vec!["/src/lib.rs"],
    "{:?}",
    status.unverified
  );
  host.clear_faults();
  let status = vol.with_host(&mut host).status(&mut store).unwrap();
  assert!(status.unverified.is_empty(), "{:?}", status.unverified);
}

/// The worked example, second half: `git pull` replaces both files on the host; drift is
/// reported on the witnessed entry and nothing else; `read_base` shows the disk; `rewitness`
/// clears the drift and leaves the agent's content alone.
#[test]
fn the_overlay_worked_example_reports_drift_and_rewitnesses() {
  let (mut host, mut store, mut vol, _) = worked_example();
  host.advance_ns(1_000_000_000);
  host.replace_file("/src/lib.rs", b"pulled lib");
  host.replace_file("/src/main.rs", b"pulled main");
  let status = vol.with_host(&mut host).status(&mut store).unwrap();
  assert_eq!(
    status.drift,
    vec![("/src/lib.rs".to_owned(), DriftKind::Replaced)]
  );
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/src/lib.rs").unwrap(),
    b"pub fn lib() {}",
    "the agent's bytes"
  );
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/src/main.rs").unwrap(),
    b"pulled main",
    "untouched shows the live disk"
  );
  assert_eq!(
    vol.with_host(&mut host).read_base("/src/lib.rs").unwrap(),
    b"pulled lib"
  );
  let redone = vol
    .with_host(&mut host)
    .rewitness(&mut store, None)
    .unwrap();
  assert_eq!(redone, vec!["/src/lib.rs".to_owned()]);
  assert!(
    vol
      .with_host(&mut host)
      .status(&mut store)
      .unwrap()
      .drift
      .is_empty()
  );
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/src/lib.rs").unwrap(),
    b"pub fn lib() {}",
    "content untouched by rewitness"
  );
}

/// AC-1.11: a large-class base file whose remaining extents are served from disk is truncated
/// in place by the user; a read of an unpinned range returns `BaseDrift`, never torn bytes, and
/// the agent's own written window is intact.
#[test]
fn an_in_place_overwrite_beneath_a_large_file_is_base_drift_never_torn_bytes() {
  let mut host = SimHost::new();
  let big: Vec<u8> = (0..(3 * LARGE))
    .map(|i| u8::try_from(i % 251).unwrap())
    .collect();
  host.replace_file("/data.bin", &big);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let f = vol
    .with_host(&mut host)
    .resolve(&mut store, "/data.bin")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .write(&mut store, f, 10, b"agent")
    .unwrap();
  let mut buf = [0u8; 16];
  assert_eq!(
    vol
      .with_host(&mut host)
      .read(&mut store, f, 8, &mut buf)
      .unwrap(),
    16
  );
  assert_eq!(&buf[2..7], b"agent");
  // Only the first window is pinned; the rest still reads through the descriptor.
  assert_eq!(
    vol
      .with_host(&mut host)
      .read(&mut store, f, 2 * LARGE, &mut buf)
      .unwrap(),
    16
  );
  assert_eq!(buf[0], u8::try_from((2 * LARGE) % 251).unwrap());

  host.write_in_place(
    "/data.bin",
    &big[..usize::try_from(LARGE).unwrap()],
    5_000_000_000,
  );
  assert_eq!(
    vol
      .with_host(&mut host)
      .read(&mut store, f, 2 * LARGE, &mut buf),
    Err(VfsError::BaseDrift)
  );
  assert_eq!(
    vol
      .with_host(&mut host)
      .read(&mut store, f, 8, &mut buf)
      .unwrap(),
    16,
    "the pinned window still reads"
  );
  assert_eq!(&buf[2..7], b"agent");
  assert_eq!(
    vol.with_host(&mut host).status(&mut store).unwrap().drift,
    vec![("/data.bin".to_owned(), DriftKind::Modified)]
  );
}

/// T-1.11: a base file modified in place within the timestamp granularity without a size
/// change; the racy rule re-hashes and the drift is detected.
#[test]
fn a_racy_in_place_edit_is_caught_by_rehashing() {
  let mut host = SimHost::new();
  host.set_granularity_ns(1_000_000_000);
  host.replace_file("/notes.txt", b"version one");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let f = vol
    .with_host(&mut host)
    .resolve(&mut store, "/notes.txt")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .chmod(&mut store, f, 0o600)
    .unwrap();
  let witness = vol.base_plane().unwrap().witness(f).unwrap();
  assert!(
    witness.racy,
    "the file's mtime is within one second of the listing"
  );
  // Same size, same timestamps: only the bytes differ.
  host.write_in_place("/notes.txt", b"version two", 0);
  assert_eq!(host.fingerprint("/notes.txt").unwrap(), witness.fingerprint);
  assert_eq!(
    vol.with_host(&mut host).status(&mut store).unwrap().drift,
    vec![("/notes.txt".to_owned(), DriftKind::Modified)]
  );
}

/// Format: a wall-clock instant at Unix scale (2023-11-14T22:13:20Z in nanoseconds), where a
/// real host's file timestamps live.
const UNIX_SCALE_NS: i64 = 1_700_000_000_000_000_000;

/// The racy rule (§4.5) is a question about the filesystem's clock: a witness is racy only when
/// the listing was read within the timestamp granularity of the file's last change *in the host's
/// clock*. With the daemon's own clock (`HostClock`, monotonic nanoseconds in the host boot domain) and a host whose timestamps sit at Unix scale, a file a whole second older than the
/// listing is not racy; comparing the two clock domains called every such witness racy and
/// re-hashed every drift check (docs/bugs/2026-09-14-racy-rule-compares-monotonic-with-wall-clock.md).
#[test]
fn a_witness_is_racy_only_within_the_hosts_own_clock() {
  let mut host = SimHost::new();
  host.set_granularity_ns(1_000);
  host.advance_ns(UNIX_SCALE_NS);
  host.replace_file("/old.txt", b"written a second ago");
  host.advance_ns(1_000_000_000);
  let mut store = store();
  let root = host.root();
  let facts = host.facts(root).unwrap();
  let mut vol = Volume::create_overlay(
    &mut store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 20,
      clock: Box::new(slates_vfs::clock::HostClock::new()),
    },
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap();
  let f = vol
    .with_host(&mut host)
    .resolve(&mut store, "/old.txt")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .chmod(&mut store, f, 0o600)
    .unwrap();
  let witness = vol.base_plane().unwrap().witness(f).unwrap();
  assert!(
    !witness.racy,
    "a file a second older than the listing, at a microsecond granularity, is not racy"
  );
}

/// T-1.12: `rm -r` of a 40k-entry base directory then recreate two files inside; expect one
/// opaque directory over the whiteout, two overlay entries, and a merged listing of exactly two.
#[test]
fn removing_a_large_base_directory_and_recreating_two_files_is_one_opaque_directory() {
  /// Format: the design's entry count for this case.
  const ENTRIES: usize = 40_000;
  let mut host = SimHost::new();
  host.mkdir("/vendor");
  for i in 0..ENTRIES {
    host.replace_file(&format!("/vendor/pkg-{i:05}.txt"), b"x");
  }
  // The tree predates the mount, as a disk tree does: its last change is older than the timestamp
  // granularity, so its listing is read once and trusted (the listing racy rule, §4.5).
  host.advance_ns(1_000_000_000);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let vendor = match vol
    .with_host(&mut host)
    .resolve(&mut store, "/vendor")
    .unwrap()
    .child
  {
    Child::Dir(h) => h,
    other => panic!("{other:?}"),
  };
  let names: Vec<String> = vol
    .with_host(&mut host)
    .readdir(&mut store, vendor)
    .unwrap()
    .iter()
    .map(|r| r.name.to_owned())
    .collect();
  assert_eq!(names.len(), ENTRIES);
  for n in &names {
    vol
      .with_host(&mut host)
      .unlink(&mut store, vendor, n)
      .unwrap();
  }
  let root = vol.root();
  vol
    .with_host(&mut host)
    .rmdir(&mut store, root, "vendor")
    .unwrap();
  let vendor = vol
    .with_host(&mut host)
    .mkdir(&mut store, root, "vendor", 0o755)
    .unwrap();
  vol
    .with_host(&mut host)
    .create_file(&mut store, vendor, "a.txt", 0o644)
    .unwrap();
  vol
    .with_host(&mut host)
    .create_file(&mut store, vendor, "b.txt", 0o644)
    .unwrap();
  let mut listed: Vec<String> = vol
    .with_host(&mut host)
    .readdir(&mut store, vendor)
    .unwrap()
    .iter()
    .map(|r| r.name.to_owned())
    .collect();
  listed.sort();
  assert_eq!(listed, vec!["a.txt".to_owned(), "b.txt".to_owned()]);
  let diverged = vol.diverged(&store);
  let kinds: Vec<(&str, Divergence)> = diverged.iter().map(|d| (d.path.as_str(), d.kind)).collect();
  assert_eq!(
    kinds,
    vec![
      ("/vendor", Divergence::Created),
      ("/vendor/a.txt", Divergence::Created),
      ("/vendor/b.txt", Divergence::Created),
    ]
  );
}

/// T-1.13: a watcher overflow is followed by a full re-check: no drift is missed and `status`
/// reports the watcher overflowed.
#[test]
fn a_watcher_overflow_rechecks_everything() {
  let mut host = SimHost::new();
  host.replace_file("/a.txt", b"a");
  host.replace_file("/b.txt", b"b");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let a = vol
    .with_host(&mut host)
    .resolve(&mut store, "/a.txt")
    .unwrap()
    .inode;
  let b = vol
    .with_host(&mut host)
    .resolve(&mut store, "/b.txt")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .write(&mut store, a, 0, b"A")
    .unwrap();
  vol
    .with_host(&mut host)
    .write(&mut store, b, 0, b"B")
    .unwrap();
  assert_eq!(
    vol.with_host(&mut host).status(&mut store).unwrap().watcher,
    WatchState::Live
  );
  host.watcher_overflow();
  host.advance_ns(1);
  host.replace_file("/b.txt", b"changed while the watcher was deaf");
  let status = vol.with_host(&mut host).status(&mut store).unwrap();
  assert_eq!(status.watcher, WatchState::Overflowed);
  assert_eq!(
    status.drift,
    vec![("/b.txt".to_owned(), DriftKind::Replaced)]
  );
}

// ------------------------------------------------------------------ the clean-file digest (§4.15)

/// Format: the BLAKE3 of the empty input, the published BLAKE3 test vector for `input_len` 0
/// (BLAKE3 specification, `test_vectors.json`).
const BLAKE3_EMPTY: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
/// Format: the BLAKE3 of the one-byte input `0x00`, the published test vector for `input_len` 1
/// (the vectors' input is the byte sequence 0, 1, 2, …, so its one-byte prefix is `0x00`).
const BLAKE3_ONE_ZERO_BYTE: &str =
  "2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213";

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest_of(
  vol: &mut Volume,
  host: &mut SimHost,
  store: &mut Store,
  path: &str,
) -> Result<slates_vfs::base::Digest, VfsError> {
  vol.with_host(host).digest(store, path)
}

/// AC-1.17 / T-1.21 (§4.15: "a verified content digest ... names exactly the current immutable
/// file bytes"): a clean base file exports the BLAKE3 of the bytes the disk holds and their
/// length, and the export does not diverge the entry; the entry the agent edited refuses
/// `DigestNotClean` rather than name bytes that are not the disk's; a directory and a missing
/// path refuse as themselves.
#[test]
fn a_clean_base_file_exports_its_verified_digest_and_a_diverged_entry_refuses() {
  let (mut host, mut store, mut vol, _) = worked_example();
  let digest = digest_of(&mut vol, &mut host, &mut store, "/src/main.rs").unwrap();
  assert_eq!(digest.identity, *blake3::hash(b"fn main() {}").as_bytes());
  assert_eq!(digest.size, 12);
  assert_eq!(
    digest_of(&mut vol, &mut host, &mut store, "/src/lib.rs"),
    Err(VfsError::DigestNotClean),
    "the agent's edit is not the disk's bytes"
  );
  assert_eq!(
    digest_of(&mut vol, &mut host, &mut store, "/src"),
    Err(VfsError::IsDirectory)
  );
  assert_eq!(
    digest_of(&mut vol, &mut host, &mut store, "/src/none.rs"),
    Err(VfsError::NotFound)
  );
  assert_eq!(
    vol.base_plane().unwrap().digest_stats().computed,
    1,
    "one digest was computed"
  );
  assert!(
    vol
      .diverged(&store)
      .iter()
      .all(|d| d.path != "/src/main.rs"),
    "a digest does not diverge the entry"
  );
}

/// §4.15 "missing or stale cache knowledge cannot produce a clean digest": an outsider replaces
/// the file with a new inode, overwrites it in place in a later tick, overwrites it again in the
/// same tick with the same length (a fingerprint that cannot tell), and removes it; every export
/// names the disk as it is at that moment, never an earlier digest.
#[test]
fn a_digest_is_never_stale_beneath_outsider_edits() {
  let mut host = SimHost::new();
  host.replace_file("/f", b"one");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let first = digest_of(&mut vol, &mut host, &mut store, "/f").unwrap();
  assert_eq!(first.identity, *blake3::hash(b"one").as_bytes());
  assert_eq!(first.size, 3);

  host.advance_ns(5);
  host.replace_file("/f", b"two");
  let replaced = digest_of(&mut vol, &mut host, &mut store, "/f").unwrap();
  assert_eq!(replaced.identity, *blake3::hash(b"two").as_bytes());

  host.write_in_place("/f", b"three", 5);
  let rewritten = digest_of(&mut vol, &mut host, &mut store, "/f").unwrap();
  assert_eq!(rewritten.identity, *blake3::hash(b"three").as_bytes());
  assert_eq!(rewritten.size, 5);

  // Same inode, same length, same tick: only the bytes differ.
  host.write_in_place("/f", b"3hree", 0);
  let racy = digest_of(&mut vol, &mut host, &mut store, "/f").unwrap();
  assert_eq!(racy.identity, *blake3::hash(b"3hree").as_bytes());

  host.remove("/f");
  assert_eq!(
    digest_of(&mut vol, &mut host, &mut store, "/f"),
    Err(VfsError::NotFound)
  );
}

/// The determinism gate and the golden vectors (§4.15; the digest is hashed on the wire): an
/// empty file and a one-byte file digest to the published BLAKE3 test vectors, a large-class file
/// digested in bounded windows equals the whole-buffer hash (the byte oracle), and two exports of
/// unchanged content are identical.
#[test]
fn a_digest_is_deterministic_and_matches_the_published_blake3_vectors() {
  let mut host = SimHost::new();
  host.replace_file("/empty", b"");
  host.replace_file("/zero", &[0u8]);
  let big: Vec<u8> = (0..(3 * LARGE))
    .map(|i| u8::try_from(i % 251).unwrap())
    .collect();
  host.replace_file("/big", &big);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);

  let empty = digest_of(&mut vol, &mut host, &mut store, "/empty").unwrap();
  assert_eq!(hex(&empty.identity), BLAKE3_EMPTY);
  assert_eq!(empty.size, 0);
  let zero = digest_of(&mut vol, &mut host, &mut store, "/zero").unwrap();
  assert_eq!(hex(&zero.identity), BLAKE3_ONE_ZERO_BYTE);
  assert_eq!(zero.size, 1);
  let large = digest_of(&mut vol, &mut host, &mut store, "/big").unwrap();
  assert_eq!(
    large.identity,
    *blake3::hash(&big).as_bytes(),
    "the windowed digest equals the whole-buffer hash"
  );
  assert_eq!(large.size, 3 * LARGE);
  let again = digest_of(&mut vol, &mut host, &mut store, "/big").unwrap();
  assert_eq!(
    again, large,
    "two exports of unchanged content are identical"
  );
}

fn stats(vol: &Volume) -> slates_vfs::base::DigestStats {
  vol.base_plane().unwrap().digest_stats()
}

fn inode_of(
  vol: &mut Volume,
  host: &mut SimHost,
  store: &mut Store,
  path: &str,
) -> slates_vfs::ids::InodeNo {
  vol.with_host(host).resolve(store, path).unwrap().inode
}

/// The reuse path: a second export of unchanged content is served from the kept digest after the
/// disk re-verified it — one hash, one reuse, one kept.
fn assert_second_export_reuses(vol: &mut Volume, host: &mut SimHost, store: &mut Store) {
  let first = digest_of(vol, host, store, "/a").unwrap();
  let again = digest_of(vol, host, store, "/a").unwrap();
  assert_eq!(first, again);
  assert_eq!(stats(vol).computed, 1, "one hash");
  assert_eq!(
    stats(vol).revalidated,
    1,
    "the second export reused the verified digest"
  );
  assert_eq!(stats(vol).cached, 1);
}

/// A content write and a metadata change each drop the kept digest before the mutation and leave
/// the entry `DigestNotClean` — never the pre-mutation digest.
fn assert_write_and_chmod_invalidate(vol: &mut Volume, host: &mut SimHost, store: &mut Store) {
  let a = inode_of(vol, host, store, "/a");
  vol.with_host(host).write(store, a, 0, b"A").unwrap();
  assert_eq!(
    digest_of(vol, host, store, "/a"),
    Err(VfsError::DigestNotClean),
    "never the pre-mutation digest"
  );
  assert_eq!(stats(vol).invalidated, 1, "dropped before the write");
  assert_eq!(stats(vol).cached, 0);

  let b = inode_of(vol, host, store, "/b");
  digest_of(vol, host, store, "/b").unwrap();
  vol.with_host(host).chmod(store, b, 0o600).unwrap();
  assert_eq!(
    digest_of(vol, host, store, "/b"),
    Err(VfsError::DigestNotClean),
    "a metadata mutation diverges the entry too"
  );
  assert_eq!(stats(vol).invalidated, 2);
}

/// A rename and an unlink drop the kept digest as the entry leaves its name.
fn assert_rename_and_unlink_invalidate(vol: &mut Volume, host: &mut SimHost, store: &mut Store) {
  digest_of(vol, host, store, "/c").unwrap();
  let root = vol.root();
  vol
    .with_host(host)
    .rename(store, root, "c", root, "c2")
    .unwrap();
  assert_eq!(stats(vol).invalidated, 3);
  assert_eq!(
    digest_of(vol, host, store, "/c2"),
    Err(VfsError::DigestNotClean)
  );
  assert_eq!(digest_of(vol, host, store, "/c"), Err(VfsError::NotFound));

  digest_of(vol, host, store, "/d").unwrap();
  vol.with_host(host).unlink(store, root, "d").unwrap();
  assert_eq!(stats(vol).invalidated, 4);
  assert_eq!(digest_of(vol, host, store, "/d"), Err(VfsError::NotFound));
}

/// §4.15 "invalidated before any mutation" over the bounded discovery cache (GAP-A9-13): a second
/// export of unchanged content is served from the cached digest once the disk has re-verified it
/// (the reuse path's counter moves, no second hash); a write drops the cached digest before the
/// write is visible and every later export refuses `DigestNotClean` — never the pre-mutation
/// digest; chmod, rename and unlink invalidate the same way, so no slot is left holding stale
/// knowledge.
#[test]
fn a_second_export_reuses_the_verified_digest_and_a_mutation_invalidates_it_first() {
  let mut host = SimHost::new();
  for (name, bytes) in [
    ("a", "alpha"),
    ("b", "bravo"),
    ("c", "charlie"),
    ("d", "delta"),
  ] {
    host.replace_file(&format!("/{name}"), bytes.as_bytes());
  }
  // Past the timestamp granularity: the files' ticks are closed, so a digest may be kept.
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  assert_second_export_reuses(&mut vol, &mut host, &mut store);
  assert_write_and_chmod_invalidate(&mut vol, &mut host, &mut store);
  assert_rename_and_unlink_invalidate(&mut vol, &mut host, &mut store);
  assert_eq!(stats(&vol).cached, 0, "no slot holds stale knowledge");
  assert_eq!(store.digests.live(), 0, "every shard slot is returned");
}

/// Digests `capacity + 1` clean files: the cache fills, the last is refused a slot; returns the
/// last file's path.
fn fill_digest_cache(
  vol: &mut Volume,
  host: &mut SimHost,
  store: &mut Store,
  capacity: usize,
) -> String {
  for index in 0..=capacity {
    digest_of(vol, host, store, &format!("/f{index}")).unwrap();
  }
  assert_eq!(stats(vol).cached, u64::try_from(capacity).unwrap());
  assert_eq!(
    stats(vol).cache_full,
    1,
    "the one past the bound was refused a slot"
  );
  assert_eq!(store.digests.live(), capacity);
  format!("/f{capacity}")
}

/// An invalidation frees a slot: the next export of `last` is kept, and the one after it reused.
fn assert_slot_freed_on_invalidation(
  vol: &mut Volume,
  host: &mut SimHost,
  store: &mut Store,
  capacity: usize,
  last: &str,
) {
  let f0 = inode_of(vol, host, store, "/f0");
  vol.with_host(host).write(store, f0, 0, b"F").unwrap();
  assert_eq!(store.digests.live(), capacity - 1);
  digest_of(vol, host, store, last).unwrap();
  assert_eq!(stats(vol).cache_full, 2, "kept this time: a slot was free");
  assert_eq!(store.digests.live(), capacity);
  digest_of(vol, host, store, last).unwrap();
  assert_eq!(stats(vol).revalidated, 1);
}

/// §4.15 "bounded cache discovery" (banned item 8): the shard's digest cache holds at most its
/// derived capacity; past it a fresh digest is still exported but not kept — the typed refusal is
/// counted and the next export hashes again — and an invalidation frees a slot, so the next
/// export is kept and then reused.
#[test]
fn the_digest_cache_refuses_at_its_derived_bound_and_frees_a_slot_on_invalidation() {
  let mut store = store();
  let capacity = store.digests.capacity();
  assert!(capacity > 0, "a store with inodes caches digests");
  // The measured point for the record (docs/wip/clean-digest.md): the fixture's derived bound.
  println!(
    "digest cache capacity: {capacity} records for max_inodes 65536 (size_of::<Inode>() = {})",
    std::mem::size_of::<slates_vfs::inode::Inode>()
  );
  let mut host = SimHost::new();
  for index in 0..=capacity {
    host.replace_file(&format!("/f{index}"), format!("file {index}").as_bytes());
  }
  host.advance_ns(2);
  let mut vol = overlay(&mut host, &mut store);
  let last = fill_digest_cache(&mut vol, &mut host, &mut store, capacity);
  digest_of(&mut vol, &mut host, &mut store, &last).unwrap();
  assert_eq!(
    stats(&vol).cache_full,
    2,
    "still refused: nothing was freed"
  );
  assert_eq!(stats(&vol).revalidated, 0);
  assert_slot_freed_on_invalidation(&mut vol, &mut host, &mut store, capacity, &last);
}

/// The racy rule (§4.5) applied to the cache: a digest computed while the file's timestamp tick is
/// still open — within the filesystem's granularity of the host's clock — could be silently
/// invalidated by a same-tick write the fingerprint cannot show, so it is exported but never kept;
/// once the tick has closed it is kept and reused.
#[test]
fn a_digest_computed_inside_the_racy_window_is_not_cached() {
  let mut host = SimHost::new();
  host.set_granularity_ns(1_000);
  host.replace_file("/f", b"fresh");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  digest_of(&mut vol, &mut host, &mut store, "/f").unwrap();
  assert_eq!(stats(&vol).racy_uncached, 1);
  assert_eq!(stats(&vol).cached, 0);
  digest_of(&mut vol, &mut host, &mut store, "/f").unwrap();
  assert_eq!(stats(&vol).computed, 2, "hashed again, never reused");
  assert_eq!(stats(&vol).revalidated, 0);

  host.advance_ns(1_001);
  digest_of(&mut vol, &mut host, &mut store, "/f").unwrap();
  assert_eq!(stats(&vol).cached, 1, "the tick has closed");
  digest_of(&mut vol, &mut host, &mut store, "/f").unwrap();
  assert_eq!(stats(&vol).revalidated, 1);
}

/// §4.15 "watcher hints backed by revalidation" (T-1.21): a hint naming a directory makes the
/// plane re-verify every digest kept beneath it against the disk — the hint triggers the check,
/// the fingerprint decides — so an outsider's replacement is dropped and the next export hashes
/// the new bytes, while the untouched neighbour survives the same hint and is reused; a later
/// hint with nothing changed beneath the kept digests (an outsider created another file) re-checks
/// and keeps them all. A hint alone never yields a digest.
#[test]
fn a_watcher_hint_revalidates_kept_digests_and_drops_only_the_changed() {
  let mut host = SimHost::new();
  host.mkdir("/d");
  host.replace_file("/d/a", b"a one");
  host.replace_file("/d/b", b"b one");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  digest_of(&mut vol, &mut host, &mut store, "/d/a").unwrap();
  digest_of(&mut vol, &mut host, &mut store, "/d/b").unwrap();
  assert_eq!(stats(&vol).cached, 2);
  assert_hint_drops_only_the_replaced(&mut vol, &mut host, &mut store);
  assert_hint_with_no_change_keeps(&mut vol, &mut host, &mut store);
}

/// An outsider replaces `a`: the watched directory hints; both kept digests beneath it are
/// re-verified, only `a`'s is dropped, `a` is hashed again and `b` is reused.
fn assert_hint_drops_only_the_replaced(vol: &mut Volume, host: &mut SimHost, store: &mut Store) {
  host.replace_file("/d/a", b"a two");
  host.advance_ns(2);
  vol.with_host(host).status(store).unwrap();
  assert_eq!(
    stats(vol).hint_rechecked,
    2,
    "both digests beneath /d were re-verified"
  );
  assert_eq!(stats(vol).stale, 1, "only the replaced one was dropped");
  assert_eq!(stats(vol).cached, 1);
  let a = digest_of(vol, host, store, "/d/a").unwrap();
  assert_eq!(a.identity, *blake3::hash(b"a two").as_bytes());
  assert_eq!(stats(vol).computed, 3, "the replaced file was hashed again");
  digest_of(vol, host, store, "/d/b").unwrap();
  assert_eq!(stats(vol).revalidated, 1, "the neighbour was reused");
  assert_eq!(stats(vol).cached, 2);
}

/// A hint with nothing changed beneath the kept digests (an outsider created `c`): both are
/// re-verified and kept, and the next export is reused.
fn assert_hint_with_no_change_keeps(vol: &mut Volume, host: &mut SimHost, store: &mut Store) {
  host.replace_file("/d/c", b"c one");
  host.advance_ns(2);
  vol.with_host(host).status(store).unwrap();
  assert_eq!(stats(vol).hint_rechecked, 4);
  assert_eq!(stats(vol).stale, 1, "nothing beneath the hint had changed");
  assert_eq!(stats(vol).cached, 2);
  digest_of(vol, host, store, "/d/b").unwrap();
  assert_eq!(stats(vol).revalidated, 2);
}

/// §4.15 "watcher overflow invalidates affected cache knowledge": on an overflow the watcher lost
/// events, so every kept digest is dropped (counted) and returned to the shard, and the next
/// export hashes again rather than trust knowledge the hints can no longer protect.
#[test]
fn a_watcher_overflow_drops_every_kept_digest() {
  let mut host = SimHost::new();
  host.replace_file("/a", b"a");
  host.replace_file("/b", b"b");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  digest_of(&mut vol, &mut host, &mut store, "/a").unwrap();
  digest_of(&mut vol, &mut host, &mut store, "/b").unwrap();
  assert_eq!(stats(&vol).cached, 2);
  host.watcher_overflow();
  vol.with_host(&mut host).status(&mut store).unwrap();
  assert_eq!(stats(&vol).dropped_on_overflow, 2);
  assert_eq!(stats(&vol).cached, 0);
  assert_eq!(store.digests.live(), 0, "every slot went back to the shard");
  digest_of(&mut vol, &mut host, &mut store, "/a").unwrap();
  assert_eq!(stats(&vol).computed, 3, "hashed again after the overflow");
  assert_eq!(stats(&vol).revalidated, 0);
}

// ------------------------------------------------------------------ the oracle (T-1.10, AC-1.10)

/// One step of a generated history: the agent's or an outsider's.
#[derive(Clone, Debug)]
enum Move {
  /// The agent writes a base or created file (creating it if absent).
  Write(u8, Vec<u8>),
  /// The agent unlinks a file.
  Unlink(u8),
  /// The agent renames a file.
  Rename(u8, u8),
  /// An outsider replaces a disk file with a new inode.
  OutsiderReplace(u8, Vec<u8>),
  /// An outsider writes a disk file in place.
  OutsiderWrite(u8, Vec<u8>),
  /// An outsider deletes a disk file.
  OutsiderRemove(u8),
}

/// Format: the file names the histories use.
const NAMES: [&str; 6] = ["a", "b", "c", "d", "e", "f"];

fn name(i: u8) -> &'static str {
  NAMES[usize::from(i) % NAMES.len()]
}

fn moves() -> impl Strategy<Value = Move> {
  prop_oneof![
    (any::<u8>(), prop::collection::vec(any::<u8>(), 1..8)).prop_map(|(i, b)| Move::Write(i, b)),
    any::<u8>().prop_map(Move::Unlink),
    (any::<u8>(), any::<u8>()).prop_map(|(a, b)| Move::Rename(a, b)),
    (any::<u8>(), prop::collection::vec(any::<u8>(), 1..8))
      .prop_map(|(i, b)| Move::OutsiderReplace(i, b)),
    (any::<u8>(), prop::collection::vec(any::<u8>(), 1..8))
      .prop_map(|(i, b)| Move::OutsiderWrite(i, b)),
    any::<u8>().prop_map(Move::OutsiderRemove),
  ]
}

/// A witness as the oracle keeps it: the disk path it was taken at, the fingerprint there,
/// and the bytes there.
#[derive(Clone, Debug)]
struct OracleWitness {
  disk_path: String,
  fingerprint: Fingerprint,
  bytes: Vec<u8>,
}

/// What the agent holds for a name.
#[derive(Clone, Debug)]
struct AgentFile {
  /// The bytes the volume holds itself (content copied up, or created); `None` while the
  /// bytes still sit on the disk behind a metadata-only witness (a renamed base file).
  bytes: Option<Vec<u8>>,
  /// The witness, when the file came from the base.
  witness: Option<OracleWitness>,
  /// Set once the volume has seen the descriptor's inode change in place: sticky until a
  /// rewitness, which these histories never do.
  lost: bool,
}

/// The oracle of (disk, overlay, witnesses) at the root directory.
#[derive(Default, Debug)]
struct Oracle {
  agent: BTreeMap<String, AgentFile>,
  /// Names the agent deleted that the disk held at the time.
  whiteouts: BTreeSet<String>,
}

impl Oracle {
  fn diverged(&self) -> Vec<(String, Divergence)> {
    let mut out: Vec<(String, Divergence)> = self
      .agent
      .iter()
      .map(|(n, f)| {
        (
          format!("/{n}"),
          if f.witness.is_some() {
            Divergence::Witnessed
          } else {
            Divergence::Created
          },
        )
      })
      .chain(
        self
          .whiteouts
          .iter()
          .map(|n| (format!("/{n}"), Divergence::Whiteout)),
      )
      .collect();
    out.sort();
    out
  }

  /// Drift: a witnessed name whose disk fingerprint, at the path the witness was taken, no
  /// longer matches the witness (a replacement with the same bytes is drift: fingerprints are
  /// the truth; identical bytes are the landing verdict's business).
  fn drift(&self, host: &SimHost) -> Vec<String> {
    self
      .agent
      .iter()
      .filter_map(|(n, f)| {
        let w = f.witness.as_ref()?;
        (host.fingerprint(&w.disk_path) != Some(w.fingerprint)).then(|| format!("/{n}"))
      })
      .collect()
  }

  /// What a read of an agent file returns: its own bytes; for bytes still on the disk, the
  /// witnessed bytes through the held descriptor, unless the descriptor's inode was changed
  /// in place, which is `BaseDrift`.
  fn expected_read(&self, host: &SimHost, n: &str) -> Option<Result<Vec<u8>, VfsError>> {
    let f = self.agent.get(n)?;
    if let Some(bytes) = &f.bytes {
      return Some(Ok(bytes.clone()));
    }
    let w = f.witness.as_ref()?;
    Some(if f.lost || Self::modified_in_place(host, w) {
      Err(VfsError::BaseDrift)
    } else {
      Ok(w.bytes.clone())
    })
  }

  /// After the volume's own check (every `status`): a file whose descriptor inode changed in
  /// place is lost from now on.
  fn observe(&mut self, host: &SimHost) {
    for f in self.agent.values_mut() {
      if f.bytes.is_none()
        && let Some(w) = &f.witness
        && Self::modified_in_place(host, w)
      {
        f.lost = true;
      }
    }
  }

  fn modified_in_place(host: &SimHost, w: &OracleWitness) -> bool {
    match host.fingerprint(&w.disk_path) {
      Some(now) => now.ino == w.fingerprint.ino && now != w.fingerprint,
      None => false,
    }
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(150), .. ProptestConfig::default() }))]

  /// T-1.10 and AC-1.10: after every step the overlay's diverged set, its witnesses and its
  /// drift list equal the oracle's, and every readable file reads what the oracle says.
  #[test]
  fn the_overlay_matches_the_oracle_under_outsider_edits(
    base in prop::collection::vec((any::<u8>(), prop::collection::vec(any::<u8>(), 1..8)), 0..6),
    history in prop::collection::vec(moves(), 1..30),
  ) {
    let mut host = SimHost::new();
    let mut base_names = BTreeSet::new();
    for (i, bytes) in &base {
      host.replace_file(&format!("/{}", name(*i)), bytes);
      base_names.insert(name(*i).to_owned());
    }
    let mut store = store();
    let mut vol = overlay(&mut host, &mut store);
    let mut oracle = Oracle::default();
    for step in &history {
      host.advance_ns(2);
      apply_move(step, &mut host, &mut store, &mut vol, &mut oracle);
      // The diverged set.
      let real: Vec<(String, Divergence)> = vol.diverged(&store).into_iter().map(|d| (d.path, d.kind)).collect();
      prop_assert_eq!(&real, &oracle.diverged(), "diverged after {:?}", step);
      // Drift.
      let status = vol.with_host(&mut host).status(&mut store).unwrap();
      oracle.observe(&host);
      let mut drift: Vec<String> = status.drift.iter().map(|(p, _)| p.clone()).collect();
      drift.sort();
      let mut expected = oracle.drift(&host);
      expected.sort();
      prop_assert_eq!(&drift, &expected, "drift after {:?}", step);
      // Bytes: the agent's files read what the oracle says; untouched base files read the disk.
      let names: Vec<String> = oracle.agent.keys().cloned().collect();
      for n in names {
        let got = read_all(&mut vol, &mut host, &mut store, &format!("/{n}"));
        prop_assert_eq!(Some(got), oracle.expected_read(&host, &n), "read of {} after {:?}", n, step);
      }
      let disk_files: Vec<String> = host
        .paths()
        .iter()
        .filter(|(_, k)| *k == HostKind::File)
        .map(|(p, _)| p.trim_start_matches('/').to_owned())
        .collect();
      for n in disk_files {
        if oracle.agent.contains_key(&n) || oracle.whiteouts.contains(&n) {
          continue;
        }
        let disk = host.bytes(&format!("/{n}"));
        let got = read_all(&mut vol, &mut host, &mut store, &format!("/{n}")).ok();
        prop_assert_eq!(got, disk, "untouched {} shows the disk after {:?}", n, step);
      }
    }
  }
}

fn apply_move(
  step: &Move,
  host: &mut SimHost,
  store: &mut Store,
  vol: &mut Volume,
  oracle: &mut Oracle,
) {
  match step {
    Move::Write(i, bytes) => agent_write(name(*i), bytes, host, store, vol, oracle),
    Move::Unlink(i) => {
      let n = name(*i);
      let root = vol.root();
      let on_disk = host.bytes(&format!("/{n}")).is_some();
      let r = vol.with_host(host).unlink(store, root, n);
      if r.is_ok() {
        oracle.agent.remove(n);
        if on_disk {
          oracle.whiteouts.insert(n.to_owned());
        }
      }
    }
    Move::Rename(a, b) => agent_rename(name(*a), name(*b), host, store, vol, oracle),
    Move::OutsiderReplace(i, bytes) => host.replace_file(&format!("/{}", name(*i)), bytes),
    Move::OutsiderWrite(i, bytes) => {
      if host.bytes(&format!("/{}", name(*i))).is_some() {
        host.write_in_place(&format!("/{}", name(*i)), bytes, 3);
      }
    }
    Move::OutsiderRemove(i) => host.remove(&format!("/{}", name(*i))),
  }
}

/// The agent writes `bytes` at offset zero of `n`, creating it when neither side has it.
fn agent_write(
  n: &str,
  bytes: &[u8],
  host: &mut SimHost,
  store: &mut Store,
  vol: &mut Volume,
  oracle: &mut Oracle,
) {
  let root = vol.root();
  let disk_now = host.bytes(&format!("/{n}"));
  let fp_now = host.fingerprint(&format!("/{n}"));
  // A file whose bytes are still on the disk behind a metadata witness cannot be pinned once
  // its inode changed in place: the write refuses with BaseDrift and nothing moves.
  let lost = oracle.agent.get(n).is_some_and(|f| {
    f.bytes.is_none()
      && (f.lost
        || f
          .witness
          .as_ref()
          .is_some_and(|w| Oracle::modified_in_place(host, w)))
  });
  let mut o = vol.with_host(host);
  let no = match o.lookup(store, root, n) {
    Ok(l) => l.inode,
    Err(VfsError::NotFound) => o.create_file(store, root, n, 0o644).unwrap(),
    Err(e) => panic!("{e:?}"),
  };
  let r = o.write(store, no, 0, bytes);
  if lost {
    assert_eq!(
      r,
      Err(VfsError::BaseDrift),
      "a lost entry refuses the write"
    );
    if let Some(f) = oracle.agent.get_mut(n) {
      f.lost = true;
    }
    return;
  }
  r.unwrap();
  let entry = oracle.agent.entry(n.to_owned()).or_insert_with(|| {
    // A base file the disk holds (and no whiteout hides) is witnessed at copy-up and starts
    // from the disk's bytes; anything else starts empty.
    let witnessed = disk_now.is_some() && !oracle.whiteouts.contains(n);
    AgentFile {
      bytes: Some(if witnessed {
        disk_now.clone().unwrap_or_default()
      } else {
        Vec::new()
      }),
      witness: witnessed.then(|| OracleWitness {
        disk_path: format!("/{n}"),
        fingerprint: fp_now.unwrap_or_default(),
        bytes: disk_now.clone().unwrap_or_default(),
      }),
      lost: false,
    }
  });
  oracle.whiteouts.remove(n);
  // Bytes still on the disk are pinned at the write: they become the volume's own.
  let current = entry
    .bytes
    .clone()
    .or_else(|| entry.witness.as_ref().map(|w| w.bytes.clone()))
    .unwrap_or_default();
  let mut next = current;
  if next.len() < bytes.len() {
    next.resize(bytes.len(), 0);
  }
  next[..bytes.len()].copy_from_slice(bytes);
  entry.bytes = Some(next);
}

/// The agent renames `from` to `to`: a base file moves with a metadata-only witness at its
/// old disk path and leaves a whiteout there.
fn agent_rename(
  from: &str,
  to: &str,
  host: &mut SimHost,
  store: &mut Store,
  vol: &mut Volume,
  oracle: &mut Oracle,
) {
  if from == to {
    return;
  }
  let root = vol.root();
  let disk_from = host.bytes(&format!("/{from}"));
  let fp_from = host.fingerprint(&format!("/{from}"));
  let r = vol.with_host(host).rename(store, root, from, root, to);
  if r.is_err() {
    return;
  }
  let moved = oracle.agent.remove(from).unwrap_or_else(|| AgentFile {
    bytes: None,
    witness: Some(OracleWitness {
      disk_path: format!("/{from}"),
      fingerprint: fp_from.unwrap_or_default(),
      bytes: disk_from.clone().unwrap_or_default(),
    }),
    lost: false,
  });
  oracle.agent.insert(to.to_owned(), moved);
  oracle.whiteouts.remove(to);
  if disk_from.is_some() {
    oracle.whiteouts.insert(from.to_owned());
  }
}

/// A clone of an overlay snapshot shares the host and the origin's witnesses.
#[test]
fn a_clone_of_an_overlay_snapshot_reads_the_same_base() {
  let mut host = SimHost::new();
  host.replace_file("/f", b"base");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let f = vol
    .with_host(&mut host)
    .resolve(&mut store, "/f")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .write(&mut store, f, 0, b"B")
    .unwrap();
  let s = vol.snapshot(&mut store).unwrap();
  let mut clone = Volume::clone_of(
    &store,
    &mut vol,
    s,
    VolumeConfig {
      prefix: 8,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(0, 1)),
    },
  )
  .unwrap();
  assert!(clone.is_overlay());
  assert!(clone.base_plane().unwrap().is_witnessed(f));
  assert_eq!(
    read_all(&mut clone, &mut host, &mut store, "/f").unwrap(),
    b"Base"
  );
  assert_eq!(clone.diverged(&store).len(), 1);
  let _ = Kind::File;
  let _ = HostKind::File;
}

/// A-38: an outsider's edit beneath an overlay moves the file's change counter when a stat takes the
/// disk's new attributes, and a stat of an unchanged file leaves it where it is, so a client caching
/// by the counter sees the edit and keeps its cache otherwise.
#[test]
fn an_outsiders_edit_moves_the_change_counter_and_a_plain_stat_does_not() {
  let mut host = SimHost::new();
  host.replace_file("/f", b"one");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let counter = |vol: &mut Volume, host: &mut SimHost, store: &mut Store| {
    let (no, size) = {
      let mut o = vol.with_host(host);
      let no = o.resolve(store, "/f").unwrap().inode;
      (no, o.stat(store, no).unwrap().size)
    };
    (size, vol.change_version(store, no).unwrap())
  };
  let (size, first) = counter(&mut vol, &mut host, &mut store);
  assert_eq!(size, 3);
  let (_, again) = counter(&mut vol, &mut host, &mut store);
  assert_eq!(
    again, first,
    "a stat of an unchanged file does not move the counter"
  );

  host.write_in_place("/f", b"three", 5);
  let (size, edited) = counter(&mut vol, &mut host, &mut store);
  assert_eq!(size, 5, "the stat took the disk's new size");
  assert!(edited > first, "the outsider's edit moved the counter");
  let (_, settled) = counter(&mut vol, &mut host, &mut store);
  assert_eq!(settled, edited, "and a further stat leaves it");
}

/// The worked example taken on to a snapshot and past it (A-48): `lib.rs` copied up under witness W1 and
/// `main.rs` deleted under a whiteout of its base fingerprint F1, then snapshot S. After it an outsider
/// replaces both files; the head sees the drift and rewitnesses `lib.rs` (W2), and creates `main.rs`
/// again and deletes it, which records the whiteout over the replaced file (F2). Returns the snapshot,
/// `lib.rs`'s inode, W1 and F1.
fn past_a_snapshot() -> (
  SimHost,
  Store,
  Volume,
  slates_vfs::ids::SnapshotId,
  slates_vfs::ids::InodeNo,
  slates_vfs::inode::Witness,
  Fingerprint,
) {
  let (mut host, mut store, mut vol, lib) = worked_example();
  let w1 = vol.base_plane().unwrap().witness(lib).unwrap();
  let f1 = host.fingerprint("/src/main.rs").unwrap();
  let src = src_dir(&mut vol, &mut host, &mut store);
  vol
    .with_host(&mut host)
    .unlink(&mut store, src, "main.rs")
    .unwrap();
  let s = vol.snapshot(&mut store).unwrap();
  host.advance_ns(1_000_000_000);
  host.replace_file("/src/lib.rs", b"pulled lib");
  host.replace_file("/src/main.rs", b"pulled main");
  vol.with_host(&mut host).status(&mut store).unwrap();
  vol
    .with_host(&mut host)
    .rewitness(&mut store, None)
    .unwrap();
  let src = src_dir(&mut vol, &mut host, &mut store);
  vol
    .with_host(&mut host)
    .create_file(&mut store, src, "main.rs", 0o100_644)
    .unwrap();
  let src = src_dir(&mut vol, &mut host, &mut store);
  vol
    .with_host(&mut host)
    .unlink(&mut store, src, "main.rs")
    .unwrap();
  (host, store, vol, s, lib, w1, f1)
}

/// The head's `/src` directory node.
fn src_dir(
  vol: &mut Volume,
  host: &mut SimHost,
  store: &mut Store,
) -> slates_mem::Handle<slates_vfs::dir::DirNode> {
  let Child::Dir(src) = vol.with_host(host).resolve(store, "/src").unwrap().child else {
    panic!("a directory");
  };
  src
}

/// A-48 (AUD-29-02's prerequisite): a snapshot keeps the witnesses it froze. Do: take the worked example
/// past a snapshot (a rewitness and a whiteout over a replaced file after it). Expect: the snapshot reads
/// W1 for `lib.rs` and F1 behind `main.rs`'s whiteout, and its diverged set is the one it froze — while
/// the head reads W2 and F2.
#[test]
fn a_snapshot_keeps_the_witnesses_it_froze_through_later_changes() {
  let (host, store, vol, s, lib, w1, f1) = past_a_snapshot();
  let head = vol.base_plane().unwrap().witness(lib).unwrap();
  assert_ne!(head, w1, "the head rewitnessed");
  assert_eq!(vol.witness_in(s, lib), Some(w1));
  assert_eq!(
    vol.whiteout_witness_in(&store, s, "/src", "main.rs"),
    Some(f1)
  );
  let f2 = host.fingerprint("/src/main.rs").unwrap();
  assert_ne!(f2, f1, "the outsider replaced main.rs");
  assert_eq!(vol.whiteout_witness(&store, "/src", "main.rs"), Some(f2));
  let frozen: Vec<(String, Divergence)> = vol
    .diverged_in(&store, s)
    .unwrap()
    .into_iter()
    .map(|d| (d.path, d.kind))
    .collect();
  assert_eq!(
    frozen,
    vec![
      ("/src/lib.rs".to_owned(), Divergence::Witnessed),
      ("/src/main.rs".to_owned(), Divergence::Whiteout),
    ]
  );
}

/// A-48 (the clone sibling): a clone starts from the witnesses of the snapshot it was made from. Do: take
/// the worked example past a snapshot and clone the snapshot. Expect: the clone reads W1 for `lib.rs` and
/// F1 behind `main.rs`'s whiteout. Before 2026-09-30 it read the head's W2 and F2.
#[test]
fn a_clone_starts_from_its_snapshots_witnesses_not_the_heads() {
  let (_host, store, mut vol, s, lib, w1, f1) = past_a_snapshot();
  let clone = Volume::clone_of(
    &store,
    &mut vol,
    s,
    VolumeConfig {
      prefix: 8,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(0, 1)),
    },
  )
  .unwrap();
  assert_eq!(clone.base_plane().unwrap().witness(lib), Some(w1));
  assert_eq!(clone.whiteout_witness(&store, "/src", "main.rs"), Some(f1));
}

/// A-48's bound: the witness versions only a snapshot read go with its destroy. Do: take the worked
/// example past a snapshot, count the versions, destroy the snapshot. Expect: more versions than the
/// head's while the snapshot lives, the head's alone after, and the head's reads unchanged.
#[test]
fn a_snapshots_destroy_drops_the_witness_versions_only_it_read() {
  let (_host, mut store, mut vol, s, lib, _w1, _hidden) = past_a_snapshot();
  let head = vol.base_plane().unwrap().witness(lib);
  let with_snapshot = vol.base_plane().unwrap().witness_versions();
  vol.destroy_snapshot(&mut store, s).unwrap();
  let after = vol.base_plane().unwrap().witness_versions();
  // The head holds lib.rs's witness, its home, and main.rs's whiteout: one version each.
  assert_eq!(after, 3, "the head's versions alone");
  assert!(with_snapshot > after, "{with_snapshot} with the snapshot");
  assert_eq!(vol.base_plane().unwrap().witness(lib), head);
}

/// A-48 (§4.8 recovery): the witness versions a snapshot reads survive the volume image. Do: take the
/// worked example past a snapshot, image the volume and recover it. Expect: the recovered snapshot reads
/// W1 and the whiteout, and the recovered head the rewitnessed witness.
#[test]
fn a_recovered_snapshot_keeps_the_witnesses_it_froze() {
  let (mut host, store, vol, s, lib, w1, f1) = past_a_snapshot();
  let head = vol.base_plane().unwrap().witness(lib);
  let image = vol.to_image(&store, Some(&mut host)).unwrap();
  let image = slates_vfs::recover::VolumeImage::from_content(&image.to_content()).unwrap();
  let mut after = common::surviving(&store);
  let root = host.root();
  let claims = common::claims(&mut after, &[&image]);
  let recovered = Volume::from_image(
    &mut after,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 20,
    Some((&mut host, root)),
  )
  .unwrap();
  assert_eq!(recovered.witness_in(s, lib), Some(w1));
  assert_eq!(
    recovered.whiteout_witness_in(&after, s, "/src", "main.rs"),
    Some(f1)
  );
  assert_eq!(recovered.base_plane().unwrap().witness(lib), head);
}

/// The names `dir` lists through the overlay, sorted.
fn listed(vol: &mut Volume, host: &mut SimHost, store: &mut Store, path: &str) -> Vec<String> {
  let dir = match vol.with_host(host).resolve(store, path).unwrap().child {
    Child::Dir(h) => h,
    other => panic!("{other:?}"),
  };
  let mut names: Vec<String> = vol
    .with_host(host)
    .readdir(store, dir)
    .unwrap()
    .iter()
    .map(|r| r.name.to_owned())
    .collect();
  names.sort();
  names
}

/// AUD-29-16 (§4.5; POSIX `rename`: a directory replaces only an empty directory): do: rename a new
/// directory over a base directory whose disk child has never been read, and again after listing it;
/// expect `NotEmpty` both times with both trees unchanged, and — once the base child is whiteouted — the
/// rename to succeed. (The hostless form is refused at the bridge: `crates/bridge-core/tests/base_overlay.rs`.) Until 2026-09-30 rename asked the overlay node alone
/// (`DirNode::is_empty`), found no entries in an unread base directory, and replaced it.
#[test]
fn a_directory_never_replaces_a_lazy_base_directory_with_children() {
  let mut host = SimHost::new();
  host.mkdir("/target");
  host.replace_file("/target/keep", b"kept");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let root = vol.root();
  vol
    .with_host(&mut host)
    .mkdir(&mut store, root, "src", 0o755)
    .unwrap();
  assert_eq!(
    vol
      .with_host(&mut host)
      .rename(&mut store, root, "src", root, "target"),
    Err(VfsError::NotEmpty),
    "before the target's children were read"
  );
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/target/keep").unwrap(),
    b"kept"
  );
  assert!(vol.with_host(&mut host).resolve(&mut store, "/src").is_ok());
  assert_eq!(
    listed(&mut vol, &mut host, &mut store, "/target"),
    vec!["keep".to_owned()]
  );
  assert_eq!(
    vol
      .with_host(&mut host)
      .rename(&mut store, root, "src", root, "target"),
    Err(VfsError::NotEmpty),
    "after listing"
  );
  let target = match vol
    .with_host(&mut host)
    .resolve(&mut store, "/target")
    .unwrap()
    .child
  {
    Child::Dir(h) => h,
    other => panic!("{other:?}"),
  };
  vol
    .with_host(&mut host)
    .unlink(&mut store, target, "keep")
    .unwrap();
  vol
    .with_host(&mut host)
    .rename(&mut store, root, "src", root, "target")
    .unwrap();
  assert!(listed(&mut vol, &mut host, &mut store, "/target").is_empty());
  assert!(
    vol
      .with_host(&mut host)
      .resolve(&mut store, "/src")
      .is_err()
  );
}

/// AUD-29-16: do: list an empty base directory, let an outsider add a file inside it, then rename a new
/// directory over it; and, on a fresh overlay whose host refuses to open the target directory, try the
/// same rename; expect `NotEmpty` for the first (the listing is re-validated against the disk, so the
/// outsider's child counts) and the host's refusal for the second — never the target taken for absent
/// and shadowed by a new directory — with the source still in place both times.
#[test]
fn a_rename_over_a_base_directory_sees_outsider_children_and_refuses_on_a_host_fault() {
  use slates_vfs::host::HostError;
  use slates_vfs::host::sim::SimVerb;
  let mut host = SimHost::new();
  host.mkdir("/empty");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let root = vol.root();
  assert!(listed(&mut vol, &mut host, &mut store, "/empty").is_empty());
  vol
    .with_host(&mut host)
    .mkdir(&mut store, root, "src", 0o755)
    .unwrap();
  host.replace_file("/empty/outsider", b"new on disk");
  assert_eq!(
    vol
      .with_host(&mut host)
      .rename(&mut store, root, "src", root, "empty"),
    Err(VfsError::NotEmpty)
  );
  assert!(vol.with_host(&mut host).resolve(&mut store, "/src").is_ok());

  let mut faulty = SimHost::new();
  faulty.mkdir("/target");
  let mut fresh_store = common::store();
  let mut fresh = overlay(&mut faulty, &mut fresh_store);
  let fresh_root = fresh.root();
  fresh
    .with_host(&mut faulty)
    .mkdir(&mut fresh_store, fresh_root, "src", 0o755)
    .unwrap();
  faulty.fail(SimVerb::OpenDir, "/target", HostError::Unavailable(5), 1);
  assert_eq!(
    fresh
      .with_host(&mut faulty)
      .rename(&mut fresh_store, fresh_root, "src", fresh_root, "target"),
    Err(VfsError::BaseUnavailable(5))
  );
  assert!(
    fresh
      .with_host(&mut faulty)
      .resolve(&mut fresh_store, "/src")
      .is_ok()
  );
  assert!(
    fresh
      .diverged(&fresh_store)
      .iter()
      .all(|d| d.path != "/target"),
    "the disk directory was not shadowed"
  );
}

/// Shape: the owner the ownership test's disk gives its entries (a user's own tree).
const DISK_UID: u32 = 501;
/// Shape: that owner's group.
const DISK_GID: u32 = 20;

/// §4.15 (owed since 2026-09 in `docs/wip/base-fuse.md` §5; found again 2026-10-05: through a macOS mount of an
/// overlay of a user's own repository, every base file showed root's, so the user could edit none of them). Do: give a
/// base directory and file an owner on the disk, overlay it, stat both through the volume, and copy the file up with a
/// write. Expect each to report the disk's owner, and the copied-up file to keep it.
#[test]
fn a_base_entry_reports_its_owner_as_the_disk_holds_it() {
  let mut host = SimHost::new();
  host.mkdir("/src");
  host.replace_file("/src/lib.rs", b"pub fn lib() {}");
  host.chown("/src", DISK_UID, DISK_GID);
  host.chown("/src/lib.rs", DISK_UID, DISK_GID);
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let mut o = vol.with_host(&mut host);
  let dir = o.resolve(&mut store, "/src").unwrap().inode;
  let file = o.resolve(&mut store, "/src/lib.rs").unwrap().inode;
  let owner = |attrs: slates_vfs::inode::Attrs| (attrs.uid, attrs.gid);
  assert_eq!(
    owner(o.stat(&mut store, dir).unwrap()),
    (DISK_UID, DISK_GID)
  );
  assert_eq!(
    owner(o.stat(&mut store, file).unwrap()),
    (DISK_UID, DISK_GID)
  );
  o.write(&mut store, file, 0, b"pub").unwrap();
  assert_eq!(
    owner(o.stat(&mut store, file).unwrap()),
    (DISK_UID, DISK_GID),
    "a copy-up keeps the owner"
  );
}

/// The directory handle a path resolves to in the overlay.
fn dir_at(
  vol: &mut Volume,
  host: &mut SimHost,
  store: &mut Store,
  path: &str,
) -> slates_mem::Handle<slates_vfs::dir::DirNode> {
  match vol.with_host(host).resolve(store, path).unwrap().child {
    Child::Dir(handle) => handle,
    other => panic!("{path} is not a directory: {other:?}"),
  }
}

/// §4.5, §4.15 (condition 5; found 2026-10-08 by `docs/wip/bench/overlay/overlay_diff.sh`: a landing of an overlay of
/// `/usr/lib` with 18 changes planned 657 more, one `symlink` per untouched base symlink). Do: overlay a base holding a
/// file and two symlinks, list and resolve them through the volume, read one link's target, then remove the other and
/// create a symlink of the volume's own. Expect the untouched link to be in no diverged set, its target the disk's, the
/// removed one a whiteout and the created one created.
#[test]
fn an_untouched_base_symlink_is_not_a_change() {
  let mut host = SimHost::new();
  host.mkdir("/lib");
  host.replace_file("/lib/libz.so.1.3", b"elf");
  host.symlink("/lib/libz.so.1", "libz.so.1.3");
  host.symlink("/lib/libz.so", "libz.so.1");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let lib = dir_at(&mut vol, &mut host, &mut store, "/lib");
  let mut o = vol.with_host(&mut host);
  let names: Vec<String> = o
    .readdir(&mut store, lib)
    .unwrap()
    .iter()
    .map(|row| row.name.to_string())
    .collect();
  assert_eq!(
    names.len(),
    3,
    "the listing shows every base entry: {names:?}"
  );
  let link = o.resolve(&mut store, "/lib/libz.so.1").unwrap().inode;
  drop(o);
  assert_eq!(&*vol.readlink(&store, link).unwrap(), "libz.so.1.3");
  assert_eq!(vol.diverged(&store), Vec::new(), "nothing was changed");
  let mut o = vol.with_host(&mut host);
  o.unlink(&mut store, lib, "libz.so").unwrap();
  o.symlink(&mut store, lib, "libz.so.mine", "libz.so.1")
    .unwrap();
  drop(o);
  let diverged: Vec<(String, Divergence)> = vol
    .diverged(&store)
    .into_iter()
    .map(|d| (d.path, d.kind))
    .collect();
  assert_eq!(
    diverged,
    vec![
      ("/lib/libz.so".to_owned(), Divergence::Whiteout),
      ("/lib/libz.so.mine".to_owned(), Divergence::Created),
    ]
  );
}

/// §4.5 (found 2026-10-08 reading `stale_entries` beside the symlink divergence bug): an untouched base entry the disk
/// no longer lists leaves a merged directory when its listing is read again, and a symlink the volume created looked
/// untouched. Do: overlay a base directory, create a symlink of the volume's own in it, then have an outsider add a
/// file there so the next listing is read afresh. Expect the volume's symlink to be listed and resolvable after it.
#[test]
fn a_symlink_the_volume_created_survives_a_fresh_listing() {
  let mut host = SimHost::new();
  host.mkdir("/src");
  host.replace_file("/src/lib.rs", b"pub fn lib() {}");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let src = dir_at(&mut vol, &mut host, &mut store, "/src");
  vol
    .with_host(&mut host)
    .symlink(&mut store, src, "current", "lib.rs")
    .unwrap();
  host.advance_ns(2);
  host.replace_file("/src/main.rs", b"fn main() {}");
  host.advance_ns(2);
  let mut o = vol.with_host(&mut host);
  let names: BTreeSet<String> = o
    .readdir(&mut store, src)
    .unwrap()
    .iter()
    .map(|row| row.name.to_string())
    .collect();
  assert_eq!(
    names,
    ["current", "lib.rs", "main.rs"]
      .map(str::to_owned)
      .into_iter()
      .collect()
  );
  let link = o.resolve(&mut store, "/src/current").unwrap().inode;
  drop(o);
  assert_eq!(&*vol.readlink(&store, link).unwrap(), "lib.rs");
}

/// §4.5, AC-2.12 (the symlink divergence fix, 2026-10-08): an untouched base symlink stays untouched across recovery.
/// Do: overlay a base holding a symlink, resolve it, image the volume and recover it. Expect the recovered volume's
/// diverged set empty and the link's target the disk's.
#[test]
fn an_untouched_base_symlink_recovers_untouched() {
  let mut host = SimHost::new();
  host.replace_file("/target", b"bytes");
  host.symlink("/link", "target");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  vol
    .with_host(&mut host)
    .resolve(&mut store, "/link")
    .unwrap();
  let image = vol.to_image(&store, Some(&mut host)).unwrap();
  let root = host.root();
  let mut after = common::surviving(&store);
  let claims = common::claims(&mut after, &[&image]);
  let mut recovered = Volume::from_image(
    &mut after,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 20,
    Some((&mut host, root)),
  )
  .unwrap();
  assert_eq!(recovered.diverged(&after), Vec::new());
  let mut o = recovered.with_host(&mut host);
  let link = o.resolve(&mut after, "/link").unwrap().inode;
  drop(o);
  assert_eq!(&*recovered.readlink(&after, link).unwrap(), "target");
}

/// §4.5 "every mutation copies the witness up" (the symlink divergence fix, 2026-10-08): renaming an untouched base
/// symlink makes it the volume's own. Do: overlay a base symlink, rename it within its directory, then have an outsider
/// change the directory so its listing is read afresh. Expect the old name a whiteout, the new name created, and the
/// renamed link still listed with the disk's target after the fresh listing.
#[test]
fn a_renamed_base_symlink_is_a_removal_and_a_creation() {
  let mut host = SimHost::new();
  host.mkdir("/lib");
  host.symlink("/lib/libz.so", "libz.so.1");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let lib = dir_at(&mut vol, &mut host, &mut store, "/lib");
  vol
    .with_host(&mut host)
    .rename(&mut store, lib, "libz.so", lib, "libz.so.old")
    .unwrap();
  host.advance_ns(2);
  host.replace_file("/lib/other", b"outsider");
  host.advance_ns(2);
  let mut o = vol.with_host(&mut host);
  let names: BTreeSet<String> = o
    .readdir(&mut store, lib)
    .unwrap()
    .iter()
    .map(|row| row.name.to_string())
    .collect();
  assert_eq!(
    names,
    ["libz.so.old", "other"]
      .map(str::to_owned)
      .into_iter()
      .collect()
  );
  let link = o.resolve(&mut store, "/lib/libz.so.old").unwrap().inode;
  drop(o);
  assert_eq!(&*vol.readlink(&store, link).unwrap(), "libz.so.1");
  let diverged: Vec<(String, Divergence)> = vol
    .diverged(&store)
    .into_iter()
    .map(|d| (d.path, d.kind))
    .collect();
  assert_eq!(
    diverged,
    vec![
      ("/lib/libz.so".to_owned(), Divergence::Whiteout),
      ("/lib/libz.so.old".to_owned(), Divergence::Created),
    ]
  );
}

/// §4.5 "an untouched entry shows the live disk" (the symlink divergence fix, 2026-10-08): a base symlink carries the
/// disk's times, and an outsider retargeting it (`ln -sf`: a new link at the name) shows through once the directory is
/// listed again. Do: overlay a base symlink and stat it, then replace it on the disk with a link to another target and
/// list the directory. Expect its times the disk's, the new target after the listing, and still nothing diverged.
#[test]
fn an_untouched_base_symlink_follows_the_disk() {
  let mut host = SimHost::new();
  // Off the clocks' zero, so a time left unset cannot equal the disk's by accident.
  host.advance_ns(5_000);
  host.mkdir("/lib");
  host.symlink("/lib/libz.so", "libz.so.1");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let lib = dir_at(&mut vol, &mut host, &mut store, "/lib");
  let mut o = vol.with_host(&mut host);
  let link = o.resolve(&mut store, "/lib/libz.so").unwrap().inode;
  let attrs = o.stat(&mut store, link).unwrap();
  drop(o);
  let disk = host.fingerprint("/lib/libz.so").unwrap();
  assert_eq!((attrs.mtime, attrs.ctime), (disk.mtime_ns, disk.ctime_ns));
  host.advance_ns(2);
  host.remove("/lib/libz.so");
  host.symlink("/lib/libz.so", "libz.so.2");
  host.advance_ns(2);
  let mut o = vol.with_host(&mut host);
  o.readdir(&mut store, lib).unwrap();
  let link = o.resolve(&mut store, "/lib/libz.so").unwrap().inode;
  drop(o);
  assert_eq!(&*vol.readlink(&store, link).unwrap(), "libz.so.2");
  assert_eq!(vol.diverged(&store), Vec::new());
}

/// §4.5 "an untouched entry shows the live disk" (the sibling of A-121's symlink retarget, 2026-10-08): an untouched
/// base file whose name the disk now gives a directory shows the directory once its parent is listed again. Do:
/// overlay a base file and resolve it, then replace it on the disk with a directory holding a file, and list the
/// parent. Expect the name to resolve to a directory, its child readable, and nothing diverged.
#[test]
fn an_untouched_base_file_the_disk_made_a_directory_shows_the_directory() {
  let mut host = SimHost::new();
  host.mkdir("/d");
  host.replace_file("/d/x", b"a file");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let d = dir_at(&mut vol, &mut host, &mut store, "/d");
  vol
    .with_host(&mut host)
    .resolve(&mut store, "/d/x")
    .unwrap();
  host.advance_ns(2);
  host.remove("/d/x");
  host.mkdir("/d/x");
  host.replace_file("/d/x/inner", b"inside");
  host.advance_ns(2);
  vol.with_host(&mut host).readdir(&mut store, d).unwrap();
  let located = vol
    .with_host(&mut host)
    .resolve(&mut store, "/d/x")
    .unwrap();
  assert!(
    matches!(located.child, Child::Dir(_)),
    "{:?}",
    located.child
  );
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/d/x/inner").unwrap(),
    b"inside"
  );
  assert_eq!(vol.diverged(&store), Vec::new());
}

/// §4.5 (with the file case above, 2026-10-08): an untouched base directory whose name the disk now gives a file shows
/// the file once its parent is listed again. Do: overlay an empty base directory and resolve it, then replace it on
/// the disk with a file, and list the parent. Expect the name to read as the file, and nothing diverged.
#[test]
fn an_untouched_base_directory_the_disk_made_a_file_shows_the_file() {
  let mut host = SimHost::new();
  host.mkdir("/d");
  host.mkdir("/d/x");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let d = dir_at(&mut vol, &mut host, &mut store, "/d");
  dir_at(&mut vol, &mut host, &mut store, "/d/x");
  host.advance_ns(2);
  host.remove("/d/x");
  host.replace_file("/d/x", b"now a file");
  host.advance_ns(2);
  vol.with_host(&mut host).readdir(&mut store, d).unwrap();
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/d/x").unwrap(),
    b"now a file"
  );
  assert_eq!(vol.diverged(&store), Vec::new());
}
/// §4.6 cache posture: an outsider's change beneath a base directory reaches the kernel caches of the volume's
/// transports through the watcher's hints, drained each reap tick. Do: load a base directory, park a counting waker on
/// the overlay volume, edit a file beneath it from outside, and drain the hints. Expect the parked task woken, so a FUSE
/// mount delivers the stale marks without waiting for its kernel's next request — and a drain with no hints to wake no one.
#[test]
fn an_outsiders_change_beneath_a_base_directory_wakes_a_task_parked_on_the_volume() {
  let mut host = SimHost::new();
  host.mkdir("/d");
  host.replace_file("/d/edited", b"one");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let _ = vol
    .with_host(&mut host)
    .resolve(&mut store, "/d/edited")
    .unwrap();
  let (counter, waker) = common::counting_waker();
  vol.with_host(&mut host).process_hints(&mut store).unwrap();
  let seen = vol.change_generation();
  assert!(
    !vol.wait_for_change(seen, &waker),
    "nothing changed yet: park"
  );
  vol.with_host(&mut host).process_hints(&mut store).unwrap();
  assert_eq!(counter.wakes(), 0, "a drain with no hints wakes no one");
  host.write_in_place("/d/edited", b"one, edited", 2);
  vol.with_host(&mut host).process_hints(&mut store).unwrap();
  assert_eq!(
    counter.wakes(),
    1,
    "the outsider's change woke the parked task"
  );
  assert!(
    vol.wait_for_change(seen, &waker),
    "and moved the generation"
  );
}

/// RFC 8881 §10.2 (A-79's gate; found 2026-10-08 reading why an outsider's edit under an NFSv4 mount might never reach
/// a delegation holder): "operations done outside the NFSv4.1 protocol ... also need to result in delegation recall".
/// An outsider editing the disk beneath an untouched base file changes what the volume serves for it, so the watcher's
/// hint must ask for the recall of the delegations held on such a file. Do: overlay a directory of two untouched files
/// and a copied-up one, delegate all three, have an outsider write the first in place, and drain the hints; then have
/// the outsider add a file there (a hint naming only the directory in the simulation) and drain again. Expect the
/// in-place write to recall the first file alone (its hint names it), the directory's hint to recall both untouched
/// files, and the copied-up one, whose bytes are the volume's own, never recalled.
#[test]
fn an_outsider_edit_beneath_an_untouched_base_file_asks_for_its_recall() {
  let mut host = SimHost::new();
  host.mkdir("/d");
  host.replace_file("/d/edited", b"one");
  host.replace_file("/d/neighbour", b"two");
  host.replace_file("/d/copied", b"three");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let mut o = vol.with_host(&mut host);
  let edited = o.resolve(&mut store, "/d/edited").unwrap().inode;
  let neighbour = o.resolve(&mut store, "/d/neighbour").unwrap().inode;
  let copied = o.resolve(&mut store, "/d/copied").unwrap().inode;
  o.write(&mut store, copied, 0, b"THREE").unwrap();
  drop(o);
  for no in [edited, neighbour, copied] {
    store.recall_gate.delegate(no.0, 7);
  }
  assert_eq!(store.recall_gate.take_requested(), Vec::new());
  host.write_in_place("/d/edited", b"one, edited", 2);
  vol.with_host(&mut host).process_hints(&mut store).unwrap();
  let asked = |store: &mut Store| -> BTreeSet<u64> {
    store
      .recall_gate
      .take_requested()
      .into_iter()
      .map(|(no, actor)| {
        assert_eq!(actor, None, "an outsider acts as no client");
        no
      })
      .collect()
  };
  assert_eq!(asked(&mut store), [edited.0].into_iter().collect());
  host.replace_file("/d/added", b"new");
  vol.with_host(&mut host).process_hints(&mut store).unwrap();
  assert_eq!(
    asked(&mut store),
    [edited.0, neighbour.0].into_iter().collect()
  );
  assert_eq!(store.recall_gate.outside_recalls(), 3);
}

/// CVE-2026-77179 (a host VMM's virtio-fs server followed a symlink swapped in at a path it reopened by name,
/// reading an arbitrary host file) for the OVERLAY's read side. slates never reopens a base path by name after it
/// has witnessed it (the content is in RAM), and the base seam opens every path component `O_DIRECTORY|O_NOFOLLOW`,
/// so a host swap of a base path never redirects the agent to host content. The RAM-volume leg is
/// `lifetime::an_open_handle_survives_unlink_and_recreate_reaching_the_original_inode`; this is the base/overlay
/// leg, and it exercises this session's symlink content-op refusal on a base symlink. Do: edit a base file
/// (witnessing it), then from the host (1) swap its content, (2) remove it, (3) swap a base file to a symlink to a
/// sensitive host path, (4) swap an intermediate directory to a symlink to a host directory. Expect: the agent
/// keeps reading its witnessed copy through (1) and (2); a read of a base path the host turned into a symlink is
/// `Invalid` (never the symlink's host target as content); a resolve through a directory the host turned into a
/// symlink is `NotDirectory` (never descended into the host target).
#[test]
fn an_overlay_never_follows_a_base_path_the_host_swaps_under_it_cve_2026_77179() {
  let mut host = SimHost::new();
  host.replace_file("/witnessed", b"ORIGINAL");
  host.mkdir("/realdir");
  host.replace_file("/realdir/f", b"inside-realdir");
  host.replace_file("/victim", b"VICTIM");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);

  // Witness `/witnessed` by editing it: its content now lives in the volume's RAM, not the host.
  let w = vol
    .with_host(&mut host)
    .resolve(&mut store, "/witnessed")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .write(&mut store, w, 0, b"EDITED!!")
    .unwrap();

  // (1) Host swaps the content underneath, (2) host removes it: the agent still reads its witnessed copy.
  host.replace_file("/witnessed", b"SWAPPED!!");
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/witnessed").unwrap(),
    b"EDITED!!",
    "a host content swap must not reach the agent's witnessed copy"
  );
  host.remove("/witnessed");
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/witnessed").unwrap(),
    b"EDITED!!",
    "a host removal must not strip the agent's witnessed copy"
  );

  // (3) Host turns a base file into a symlink to a sensitive host path: the agent's read is refused, never the
  // symlink's host target returned as content (this session's symlink content-op refusal, on a base symlink).
  host.remove("/victim");
  host.symlink("/victim", "/etc/passwd");
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/victim"),
    Err(VfsError::Invalid),
    "a base path the host swapped into a symlink must refuse the read, never return its host target as content"
  );

  // (4) Host turns an intermediate directory into a symlink to a host directory: the resolve does not descend
  // through it into the host target.
  host.remove("/realdir/f");
  host.remove("/realdir");
  host.symlink("/realdir", "/etc");
  assert_eq!(
    vol
      .with_host(&mut host)
      .resolve(&mut store, "/realdir/f")
      .map(|located| located.inode),
    Err(VfsError::NotDirectory),
    "a directory the host swapped into a symlink must not be descended into the host target"
  );
}

/// The canonical overlay / container escape (CWE-59, symlink following out of the lower directory): a base
/// directory that LEGITIMATELY holds a symlink whose target escapes the overlaid subtree — an absolute host path
/// (`/etc/passwd`) or a `../../..` climb — must never be followed by the overlay's own resolver into the host
/// filesystem. slates resolves one path component at a time and stops at a symlink: a symlink as a non-terminal
/// component is `NotDirectory` (never walked through), and a symlink as the terminal component is returned for the
/// caller to `readlink`, so the kernel does any following — against the mount, as the agent's own uid, the control
/// plane being same-uid (`crates/ipc/src/rendezvous.rs` refuses a peer whose `SO_PEERCRED` uid differs). This is
/// the STATIC companion to the dynamic host-swap case (`...cve_2026_77179`): there the host swaps a symlink in
/// under a witnessed path; here the base holds the escaping symlinks from the start and they are reached through
/// ordinary resolution. Do: overlay a base with `evil -> /etc/passwd`, `up -> ../../../../etc`, and a real
/// `d/inner`. Expect: resolving `evil`/`up` returns the symlink terminal and `readlink` returns the RAW target
/// (slates never opens it); resolving THROUGH either (`evil/x`, `up/x`, `evil/../etc`) is `NotDirectory`; a content
/// read of an escaping symlink is `Invalid` (this session's symlink content-op refusal, reaching a `BaseLink`); a
/// literal `..` component is `NotFound` (slates does not climb — the kernel resolves `..` before a request
/// arrives); and a real base path still resolves and reads (non-vacuity).
#[test]
fn an_overlay_never_follows_a_base_symlink_out_of_the_overlaid_subtree_cwe_59() {
  let mut host = SimHost::new();
  host.symlink("/evil", "/etc/passwd");
  host.symlink("/up", "../../../../etc");
  host.mkdir("/d");
  host.replace_file("/d/inner", b"legitimate");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);

  // Each escaping symlink resolves as a TERMINAL component; readlink returns the RAW target, which slates reports
  // and never opens. The kernel, not slates, resolves the target (against the mount, as the agent's own uid).
  let evil = vol
    .with_host(&mut host)
    .resolve(&mut store, "/evil")
    .unwrap()
    .inode;
  assert_eq!(&*vol.readlink(&store, evil).unwrap(), "/etc/passwd");
  let up = vol
    .with_host(&mut host)
    .resolve(&mut store, "/up")
    .unwrap()
    .inode;
  assert_eq!(&*vol.readlink(&store, up).unwrap(), "../../../../etc");

  // Resolving THROUGH either symlink is refused NotDirectory — the resolver stops at a symlink component and never
  // walks it into the host filesystem (no /etc/passwd, no climb above the overlaid root).
  for path in [
    "/evil/anything",
    "/up/anything",
    "/evil/../etc",
    "/up/passwd",
  ] {
    assert_eq!(
      vol
        .with_host(&mut host)
        .resolve(&mut store, path)
        .map(|l| l.inode),
      Err(VfsError::NotDirectory),
      "resolving through a base symlink must not follow it into the host: {path}"
    );
  }

  // A content read of an escaping base symlink is Invalid (the symlink content-op refusal reaches a BaseLink): the
  // symlink's target bytes are never returned as file content, only through readlink.
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/evil"),
    Err(VfsError::Invalid),
    "a content read of a base symlink must refuse, never return its target as content"
  );

  // A literal `..` path component is a name lookup, not a climb: slates does not implement `..` (the kernel
  // resolves it before a request arrives), so it is NotFound — it can never walk above the overlaid root.
  assert_eq!(
    vol
      .with_host(&mut host)
      .resolve(&mut store, "/../etc")
      .map(|l| l.inode),
    Err(VfsError::NotFound),
    "a literal `..` component must not climb above the overlaid root"
  );

  // Non-vacuity: a real base path still resolves and reads, so the refusals above are symlink/escape-specific.
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/d/inner").unwrap(),
    b"legitimate"
  );
}
