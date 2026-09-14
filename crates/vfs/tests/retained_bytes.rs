//! §4.2 all-cost charging, the retained-content dimension (GAP-A9-1): a chunk a snapshot keeps
//! alive after the head overwrote it is physical arena capacity that is neither in the volume's
//! `referenced_bytes` (the head no longer reaches it) nor in a bounded volume's reservation. Left
//! uncharged, snapshot-and-overwrite cycles let one volume hold `(snapshots + 1) × quota` of the
//! shard's arena while the budget still shows `quota` committed, and a neighbour writing within its
//! admitted entitlement finds the arena exhausted — the "retained bytes can defeat the cap" finding.
//! These tests drive snapshot-and-diverge against a bare store (no server-side reservation), so the
//! budget's committed bytes are the retention charge alone and must equal the drift-free
//! `retained_bytes` at every step, and a retention that would spend promised capacity is refused.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{PAGE, REGION_PAGES, store, volume};
use slates_vfs::error::VfsError;
use slates_vfs::volume::{Store, Volume};

/// A file holding `windows` whole chunk windows of `fill`, written window by window.
fn write_windows(vol: &mut Volume, store: &mut Store, no: slates_vfs::ids::InodeNo, windows: u64, fill: u8) {
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let block = vec![fill; usize::try_from(chunk).unwrap()];
  for window in 0..windows {
    vol.write(store, no, window * chunk, &block).unwrap();
  }
}

/// A snapshot keeps a chunk alive that the head shares with the retained inode version; destroying
/// the snapshot must free only what the head no longer reaches. Do: write two windows, snapshot,
/// overwrite window 0 only, destroy the snapshot. Expect: window 1 (never rewritten, still the
/// head's) reads back intact.
#[test]
fn destroying_a_snapshot_keeps_the_windows_the_head_still_reaches() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'a');
  let chunk = store.content.chunk_bytes();
  let snap = vol.snapshot(&mut store).unwrap();
  // Rewrite window 0 only: window 1's chunk stays shared between the head and the retained version.
  let block = vec![b'c'; chunk];
  vol.write(&mut store, f, 0, &block).unwrap();
  vol.destroy_snapshot(&mut store, snap).unwrap();
  let mut back = vec![0u8; chunk];
  let read = vol
    .read(&store, f, u64::try_from(chunk).unwrap(), &mut back)
    .unwrap();
  assert_eq!(read, chunk, "window 1 is still the file's");
  assert!(
    back.iter().all(|b| *b == b'a'),
    "window 1 must survive the snapshot's destroy (it was never rewritten): first bytes {:?}",
    &back[..8]
  );
  let mut front = vec![0u8; chunk];
  vol.read(&store, f, 0, &mut front).unwrap();
  assert!(front.iter().all(|b| *b == b'c'), "window 0 holds the rewrite");
}

/// T-A9 (byte retention): the retention charge equals the retained content bytes at every step of
/// snapshot-and-overwrite and snapshot destruction. With no server-side reservation, `committed`
/// is the retention charge alone, so `committed == retained_bytes` is the charge/credit balance.
#[test]
fn the_retention_charge_balances_the_retained_content_bytes() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let balance = |store: &Store, vol: &Volume| {
    assert_eq!(
      store.budget.committed(),
      vol.retained_bytes(store),
      "the shard budget's committed bytes are exactly the retained content bytes"
    );
  };
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'1');
  balance(&store, &vol);
  assert_eq!(vol.retained_bytes(&store), 0, "no snapshot, nothing retained");

  // A snapshot alone retains nothing; the divergence after it does — one window's chunk.
  let s1 = vol.snapshot(&mut store).unwrap();
  balance(&store, &vol);
  assert_eq!(store.budget.committed(), 0);
  let block = vec![b'2'; usize::try_from(chunk).unwrap()];
  vol.write(&mut store, f, 0, &block).unwrap();
  balance(&store, &vol);
  assert_eq!(
    store.budget.committed(),
    chunk,
    "window 0's pre-snapshot chunk is retained and charged at its block length"
  );
  // The second window diverges too.
  vol.write(&mut store, f, chunk, &block).unwrap();
  balance(&store, &vol);
  assert_eq!(store.budget.committed(), 2 * chunk);
  // An in-epoch rewrite of window 0 retains nothing new (the chunk it replaces was born after the
  // snapshot, so it is freed at once).
  vol.write(&mut store, f, 0, &block).unwrap();
  balance(&store, &vol);
  assert_eq!(store.budget.committed(), 2 * chunk);

  // Destroying the snapshot frees both retained chunks and returns the charge.
  vol.destroy_snapshot(&mut store, s1).unwrap();
  balance(&store, &vol);
  assert_eq!(
    store.budget.committed(),
    0,
    "destroying the snapshot returns the charge"
  );
}

/// T-A9 (byte retention): destroying a whole volume returns its retention charge — the credit path
/// `destroy_snapshot` does not cover. The charge settles at `destroy`, before `destroy_step` frees
/// the chunks in slices.
#[test]
fn destroying_a_volume_returns_its_byte_retention_charge() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let chunk = usize::try_from(store.content.chunk_bytes()).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'1');
  vol.snapshot(&mut store).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'2');
  assert_eq!(
    store.budget.committed(),
    u64::try_from(2 * chunk).unwrap(),
    "two chunks retained and charged"
  );
  vol.destroy(&mut store).unwrap();
  assert_eq!(
    store.budget.committed(),
    0,
    "destroy returns the whole volume's byte retention charge"
  );
}

/// AC-2.11 (T-2.13): an admitted bounded claim remains spendable against a snapshotting neighbour.
/// Do: B reserves a quarter of the shard; A (a quarter too) snapshots and overwrites its content
/// repeatedly — each cycle retains another quarter of the arena. Expect: A's retention draws only
/// the unpromised half, the cycle that would spend B's reservation is refused `NoSpace` before
/// anything changes, and B then writes its whole allowance with every chunk landing. Non-vacuous:
/// with retention uncharged, A's third cycle succeeds and B's write meets an exhausted arena.
#[test]
fn a_bounded_volume_keeps_its_entitlement_against_a_snapshotting_neighbour() {
  let mut store = store();
  let capacity = store.budget.capacity();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let quarter = capacity / 4 / chunk * chunk;
  let windows = quarter / chunk;
  // B's reservation is taken the way the server takes it: from the shard budget before any write.
  let b_reservation = store.budget.reserve(quarter).expect("B reserves a quarter");
  let a_reservation = store.budget.reserve(quarter).expect("A reserves a quarter");
  let mut a = volume(&mut store, quarter);
  let root = a.root_inode(&store).unwrap();
  let f = a.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut a, &mut store, f, windows, b'0');

  // Cycles 1 and 2 retain a quarter each — the unpromised half. Cycle 3 would retain into B's
  // (or A's own) promised space and is refused, whole, at the first window.
  let block = vec![b'x'; usize::try_from(chunk).unwrap()];
  let mut snapshots = Vec::new();
  for cycle in 0..2u64 {
    snapshots.push(a.snapshot(&mut store).unwrap());
    for window in 0..windows {
      a.write(&mut store, f, window * chunk, &block)
        .unwrap_or_else(|e| panic!("cycle {cycle} window {window} within unpromised space: {e:?}"));
    }
  }
  assert_eq!(
    store.budget.admittable(),
    0,
    "the unpromised half is spent on retention"
  );
  snapshots.push(a.snapshot(&mut store).unwrap());
  assert!(
    matches!(a.write(&mut store, f, 0, &block), Err(VfsError::NoSpace)),
    "a retention into promised space is refused"
  );
  assert_eq!(
    store.budget.committed(),
    2 * quarter + 2 * quarter,
    "the refused write changed nothing: two reservations plus two cycles of retention"
  );

  // B writes its whole allowance: every chunk lands, because retention never spent B's share.
  let mut b = volume(&mut store, quarter);
  let b_root = b.root_inode(&store).unwrap();
  let g = b.create_file_no(&mut store, b_root, "g", 0o644).unwrap();
  for window in 0..windows {
    b.write(&mut store, g, window * chunk, &block)
      .unwrap_or_else(|e| panic!("B's window {window} is within its reservation: {e:?}"));
  }
  assert_eq!(
    b.accounting().referenced_bytes,
    quarter,
    "B used its whole advertised allowance"
  );
  // Teardown returns everything.
  store.budget.release(a_reservation);
  store.budget.release(b_reservation);
  let _ = (PAGE, REGION_PAGES);
}
