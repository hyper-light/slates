//! The §4.2 retention charge: a snapshot-retained inode version draws a slot from the shard's
//! version budget, from capacity not promised to any volume's logical allowance, and returns it when
//! the version is freed. These tests drive snapshot-and-diverge against a bare store (no server-side
//! logical reservation), so the budget's committed slots are the retention charge alone and must
//! equal the drift-free `retained_versions` at every step — the charge/credit balance oracle — and
//! a retention that would dip into promised space is refused.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::error::VfsError;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

const PAGE: usize = 4096;

fn store(max_inodes: usize) -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * 512, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 64,
      max_dirs: max_inodes,
      max_inodes,
      max_chunks: 1 << 12,
      max_dir_blocks: max_inodes,
      dir_cutover: 4,
    },
    arena,
    0,
  )
}

fn volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 20 },
      journal_bytes: 1 << 16,
      clock: Box::new(StepClock::new(0, 1)),
    },
  )
  .unwrap()
}

/// T-A9: the retention charge equals the retained-version count at every step of snapshot-and-diverge
/// and snapshot destruction. With no server-side logical reservation, `committed` is the retention
/// charge alone, so `committed == retained_versions` is the charge/credit balance.
#[test]
fn the_retention_charge_balances_the_retained_version_count() {
  let mut store = store(1 << 12);
  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let balance = |store: &Store, vol: &Volume| {
    assert_eq!(
      store.versions.committed(),
      vol.retained_versions(),
      "the version budget's committed slots are exactly the retained versions"
    );
  };
  balance(&store, &vol);

  let a = vol.create_file_no(&mut store, root, "a", 0o644).unwrap();
  let b = vol.create_file_no(&mut store, root, "b", 0o644).unwrap();
  vol.write(&mut store, a, 0, b"1").unwrap();
  vol.write(&mut store, b, 0, b"1").unwrap();
  balance(&store, &vol);
  assert_eq!(vol.retained_versions(), 0, "no snapshot, nothing retained");

  // A snapshot alone retains nothing; the divergence after it does.
  let s1 = vol.snapshot(&mut store).unwrap();
  balance(&store, &vol);
  assert_eq!(store.versions.committed(), 0);

  vol.write(&mut store, a, 0, b"2").unwrap();
  balance(&store, &vol);
  assert_eq!(
    store.versions.committed(),
    1,
    "a's pre-snapshot version is charged"
  );

  vol.write(&mut store, b, 0, b"2").unwrap();
  balance(&store, &vol);
  assert_eq!(store.versions.committed(), 2);

  // A second write to a in the same epoch does not retain again (already current).
  vol.write(&mut store, a, 0, b"3").unwrap();
  balance(&store, &vol);
  assert_eq!(
    store.versions.committed(),
    2,
    "an in-epoch rewrite retains nothing new"
  );

  // Destroying the snapshot frees both retained versions and returns the charge.
  vol.destroy_snapshot(&mut store, s1).unwrap();
  balance(&store, &vol);
  assert_eq!(
    store.versions.committed(),
    0,
    "destroying the snapshot returns the charge"
  );
}

/// T-A9: destroying a whole volume returns its retention charge (the credit path `destroy_snapshot`
/// does not cover). The charge settles at `destroy`, before `destroy_step` frees the slots.
#[test]
fn destroying_a_volume_returns_its_retention_charge() {
  let mut store = store(1 << 12);
  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let a = vol.create_file_no(&mut store, root, "a", 0o644).unwrap();
  let b = vol.create_file_no(&mut store, root, "b", 0o644).unwrap();
  vol.write(&mut store, a, 0, b"1").unwrap();
  vol.write(&mut store, b, 0, b"1").unwrap();
  vol.snapshot(&mut store).unwrap();
  vol.write(&mut store, a, 0, b"2").unwrap();
  vol.write(&mut store, b, 0, b"2").unwrap();
  assert_eq!(
    store.versions.committed(),
    2,
    "two versions retained and charged"
  );
  vol.destroy(&mut store).unwrap();
  assert_eq!(
    store.versions.committed(),
    0,
    "destroy returns the whole volume's retention charge"
  );
}

/// T-A9: a retention is refused when it would dip into promised space — the reserved logical
/// allowances and the copy-up headroom (§4.2). Non-vacuous: the refused write leaves the charge at
/// exactly the retentions that the unpromised capacity could back.
#[test]
fn a_retention_refuses_when_it_would_use_promised_space() {
  let mut store = store(64);
  // Promise all but two slots to a logical reservation (the budget arithmetic already keeps the
  // one-slot copy-up headroom free), so retention has room for exactly two versions.
  let leave_for_retention = 2;
  let promise = store.versions.admittable() - leave_for_retention;
  store.versions.reserve(promise).unwrap();
  assert_eq!(store.versions.admittable(), leave_for_retention);

  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let files: Vec<_> = ["a", "b", "c"]
    .iter()
    .map(|name| {
      let f = vol.create_file_no(&mut store, root, name, 0o644).unwrap();
      vol.write(&mut store, f, 0, b"x").unwrap();
      f
    })
    .collect();
  vol.snapshot(&mut store).unwrap();

  // The first two divergences each retain a version — the two unpromised slots.
  vol.write(&mut store, files[0], 0, b"y").unwrap();
  vol.write(&mut store, files[1], 0, b"y").unwrap();
  assert_eq!(vol.retained_versions(), 2);
  assert_eq!(
    store.versions.admittable(),
    0,
    "the unpromised capacity is spent"
  );

  // The third would retain a version in promised space, so it is refused — the writer's reservation
  // and the copy-up headroom are protected.
  assert!(
    matches!(
      vol.write(&mut store, files[2], 0, b"y"),
      Err(VfsError::NoSpace)
    ),
    "a retention cannot use promised space"
  );
  assert_eq!(
    vol.retained_versions(),
    2,
    "the refused write retained nothing new"
  );
}
