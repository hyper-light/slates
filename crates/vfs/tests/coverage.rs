//! Tests for a snapshot's completeness as an immutable base (§4.4 `SnapshotCoverage`; §4.16 and
//! the A-9 integration requirement: a green "starts from scratch or a complete immutable base,
//! never an implicitly live host directory"; AC-6.13). Over the simulated host: a scratch volume's
//! snapshot is complete; an overlay's is not while any base entry is served live (unlisted, or
//! listed but unpinned); pinning the whole base and snapshotting again makes it complete, and a
//! snapshot read of a pinned base-backed file serves its bytes (an unpinned one refuses typed rather
//! than reading zeros).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_vfs::base::BaseConfig;
use slates_vfs::clock::StepClock;
use slates_vfs::error::VfsError;
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::SimHost;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, Volume, VolumeConfig};

mod common;
use common::store;

/// Shape: the large-file class boundary; every file here is far below it (small class, pinned whole).
const LARGE: u64 = 65_536;

fn config() -> VolumeConfig {
  VolumeConfig {
    prefix: 7,
    names: NameEquivalence::Exact,
    quota: Quota::Bounded { limit: 1 << 30 },
    journal_bytes: 1 << 20,
    clock: Box::new(StepClock::new(1_000_000, 1_000)),
  }
}

fn overlay(host: &mut SimHost, store: &mut Store) -> Volume {
  let root = host.root();
  let facts = host.facts(root).unwrap();
  Volume::create_overlay(
    store,
    config(),
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap()
}

/// A scratch volume's snapshot covers its whole tree: complete by construction.
#[test]
fn a_scratch_snapshot_is_complete() {
  let mut store = store();
  let mut vol = Volume::create(&mut store, config()).unwrap();
  let root = vol.root();
  let f = vol.create_file(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, f, 0, b"bytes").unwrap();
  let snap = vol.snapshot(&mut store).unwrap();
  assert!(vol.snapshot_is_complete(&store, snap).unwrap());
}

/// An overlay's snapshot is incomplete while any entry is served live — before the base was listed,
/// and after a listing but before the files were pinned — and complete once the whole base is pinned
/// and a snapshot taken after it; a snapshot taken *before* the pin stays incomplete (the rule is
/// stated on the frozen tree, not on the live tables).
#[test]
fn an_overlays_snapshot_is_complete_only_once_the_whole_base_is_pinned_before_it() {
  let mut host = SimHost::new();
  host.mkdir("/d");
  host.replace_file("/d/f", b"disk bytes");
  host.replace_file("/g", b"more");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);

  let unlisted = vol.snapshot(&mut store).unwrap();
  assert!(
    !vol.snapshot_is_complete(&store, unlisted).unwrap(),
    "an unlisted base shows the live disk"
  );

  // List the root and the subdirectory (a `readdir` through the host), pinning nothing.
  {
    let mut o = vol.with_host(&mut host);
    let root = o.resolve(&mut store, "/").unwrap();
    let slates_vfs::dir::Child::Dir(root_dir) = root.child else {
      panic!("root");
    };
    let _ = o.readdir(&mut store, root_dir).unwrap();
    let d = o.resolve(&mut store, "/d").unwrap();
    let slates_vfs::dir::Child::Dir(d_dir) = d.child else {
      panic!("d");
    };
    let _ = o.readdir(&mut store, d_dir).unwrap();
  }
  let listed = vol.snapshot(&mut store).unwrap();
  assert!(
    !vol.snapshot_is_complete(&store, listed).unwrap(),
    "listed but unpinned files still depend on the host"
  );

  let pinned = vol.with_host(&mut host).pin(&mut store, None).unwrap();
  assert_eq!(pinned, 2, "both files pinned");
  assert!(
    !vol.snapshot_is_complete(&store, listed).unwrap(),
    "a snapshot frozen before the pin is still what it was"
  );
  let complete = vol.snapshot(&mut store).unwrap();
  assert!(
    vol.snapshot_is_complete(&store, complete).unwrap(),
    "the whole base pinned before the freeze: complete"
  );

  // A snapshot read of the pinned base file serves the bytes the host held when it was pinned, and
  // keeps serving them after the host changes — the snapshot is immutable.
  let located = vol.resolve_in(&store, complete, "/d/f").unwrap();
  let mut buf = vec![0u8; 16];
  let n = vol
    .read_in(&store, complete, located.inode, 0, &mut buf)
    .unwrap();
  assert_eq!(&buf[..n], b"disk bytes");
  host.replace_file("/d/f", b"CHANGED!");
  let n = vol
    .read_in(&store, complete, located.inode, 0, &mut buf)
    .unwrap();
  assert_eq!(&buf[..n], b"disk bytes", "the frozen bytes, not the disk's");

  // The listed-but-unpinned snapshot refuses a read of its base file typed, never zeros as content.
  let stale = vol.resolve_in(&store, listed, "/d/f").unwrap();
  assert!(
    matches!(
      vol.read_in(&store, listed, stale.inode, 0, &mut buf),
      Err(VfsError::BaseUnavailable(_))
    ),
    "an unpinned base range in a snapshot needs the host"
  );
}
