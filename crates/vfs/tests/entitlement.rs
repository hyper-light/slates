//! §4.2 "Sacred bounded claims" through resize and recovery (GAP-A9-1, AC-2.11): an admitted claim
//! backs the whole admitted quota, not only current usage; grow reserves the additional entitlement
//! atomically and shrink refuses below current use; a new claim is refused before an admitted one
//! is starved; and a restart re-establishes an admitted claim — its reservation and the bytes its
//! snapshots retain — ahead of any new one. These tests drive the volume and the shard budget the
//! way the server does (a reservation taken before the volume exists, re-taken before a rebuild)
//! and assert what lands and what is refused.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{store, volume};
use slates_mem::MemError;
use slates_vfs::clock::StepClock;
use slates_vfs::error::VfsError;
use slates_vfs::ids::InodeNo;
use slates_vfs::volume::{Store, Volume};

/// Writes `windows` whole chunk windows of `fill` into `no`, from window `first`.
fn write_windows(
  vol: &mut Volume,
  store: &mut Store,
  no: InodeNo,
  first: u64,
  windows: u64,
  fill: u8,
) {
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let block = vec![fill; usize::try_from(chunk).unwrap()];
  for window in first..first + windows {
    vol
      .write(store, no, window * chunk, &block)
      .unwrap_or_else(|e| panic!("window {window} is within the claim: {e:?}"));
  }
}

/// Shrink refuses below current use and changes nothing; the claim stays spendable at its old
/// limit afterwards. Do: hold two windows, resize to one. Expect: `NoSpace`, the accounting and the
/// arena unchanged, and a third window (within the old limit) still lands.
#[test]
fn a_resize_below_current_use_is_refused_and_the_claim_stays_spendable() {
  let mut store = store();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let mut vol = volume(&mut store, 3 * chunk);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 0, 2, b'r');
  let before = (vol.accounting(), store.content.allocated_bytes());
  assert_eq!(
    vol.resize(chunk),
    Err(VfsError::NoSpace),
    "a shrink below the two windows held is refused"
  );
  assert_eq!(
    (vol.accounting(), store.content.allocated_bytes()),
    before,
    "the refused shrink changed nothing"
  );
  write_windows(&mut vol, &mut store, f, 2, 1, b'r');
  assert_eq!(
    vol.accounting().referenced_bytes,
    3 * chunk,
    "the third window, within the admitted limit, lands"
  );
  assert_eq!(
    vol.resize(2 * chunk),
    Err(VfsError::NoSpace),
    "and a shrink below three windows is refused too"
  );
}

/// New claims are refused before an admitted one is starved. Do: B and A each hold a quarter; A
/// fills half its claim, then snapshots and overwrites it until retention has taken the unpromised
/// half. Expect: a further claim is refused `BudgetExceeded`, A still lands the other half of its
/// own quota (fresh windows, nothing retained), and B lands its whole quarter.
#[test]
fn new_claims_are_refused_before_an_admitted_claim_is_starved() {
  let mut store = store();
  let capacity = store.budget.capacity();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let quarter = capacity / 4 / chunk * chunk;
  let half_claim = quarter / 2 / chunk;
  let b_claim = store.budget.reserve(quarter).unwrap();
  let a_claim = store.budget.reserve(quarter).unwrap();
  let mut a = volume(&mut store, quarter);
  let root = a.root_inode(&store).unwrap();
  let f = a.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut a, &mut store, f, 0, half_claim, b'0');
  // Each cycle retains half a quarter; four cycles spend the unpromised half.
  for cycle in 1..=4u8 {
    a.snapshot(&mut store).unwrap();
    write_windows(&mut a, &mut store, f, 0, half_claim, cycle);
  }
  assert_eq!(
    store.budget.admittable(),
    0,
    "the unpromised half is retained"
  );
  assert!(
    matches!(
      store.budget.reserve(chunk),
      Err(MemError::BudgetExceeded { available: 0, .. })
    ),
    "a new claim is refused before any admitted claim is touched"
  );
  // A's own entitlement is intact: the other half of its quota lands in fresh windows.
  write_windows(&mut a, &mut store, f, half_claim, half_claim, b'a');
  assert_eq!(a.accounting().referenced_bytes, quarter);
  // B's whole claim lands.
  let mut b = volume(&mut store, quarter);
  let b_root = b.root_inode(&store).unwrap();
  let g = b.create_file_no(&mut store, b_root, "g", 0o644).unwrap();
  write_windows(&mut b, &mut store, g, 0, quarter / chunk, b'b');
  assert_eq!(b.accounting().referenced_bytes, quarter);
  store.budget.release(a_claim);
  store.budget.release(b_claim);
}

/// A restart re-establishes an admitted claim ahead of new ones. Do: A holds a claim with content
/// and a snapshot retaining a rewritten window; capture its image; on a fresh shard take B's claim
/// and A's claim again, then rebuild A (its retention re-charged by the rebuild). Expect: the
/// ledger holds both claims plus A's retention, a claim for the remainder plus one window is
/// refused, A still writes within its claim, and B lands its whole claim.
#[test]
fn recovery_re_establishes_an_admitted_claim_ahead_of_new_ones() {
  let mut source = store();
  let chunk = u64::try_from(source.content.chunk_bytes()).unwrap();
  let capacity = source.budget.capacity();
  let quarter = capacity / 4 / chunk * chunk;
  let mut vol = volume(&mut source, quarter);
  let root = vol.root_inode(&source).unwrap();
  let f = vol.create_file_no(&mut source, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut source, f, 0, 2, b'1');
  vol.snapshot(&mut source).unwrap();
  write_windows(&mut vol, &mut source, f, 0, 1, b'2');
  let image = vol.to_image(&source, None).unwrap();

  let mut fresh = store();
  let b_claim = fresh.budget.reserve(quarter).unwrap();
  let a_claim = fresh.budget.reserve(quarter).unwrap();
  let mut recovered = Volume::from_image(
    &mut fresh,
    &image,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  let retained = recovered.retained_bytes(&fresh);
  assert!(
    retained > 0,
    "the rebuilt snapshot retains its private copy"
  );
  assert_eq!(
    fresh.budget.committed(),
    2 * quarter + retained,
    "both claims and A's retention are on the ledger after the rebuild"
  );
  let remainder = fresh.budget.admittable();
  assert!(
    matches!(
      fresh.budget.reserve(remainder + chunk),
      Err(MemError::BudgetExceeded { .. })
    ),
    "a claim that would need A's retained bytes is refused"
  );
  // A writes within its claim after the restart; B lands its whole claim.
  write_windows(&mut recovered, &mut fresh, f, 2, 1, b'3');
  let mut b = volume(&mut fresh, quarter);
  let b_root = b.root_inode(&fresh).unwrap();
  let g = b.create_file_no(&mut fresh, b_root, "g", 0o644).unwrap();
  write_windows(&mut b, &mut fresh, g, 0, quarter / chunk, b'b');
  assert_eq!(b.accounting().referenced_bytes, quarter);
  fresh.budget.release(a_claim);
  fresh.budget.release(b_claim);
}
