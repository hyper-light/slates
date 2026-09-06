//! §4.2 admission accounting through the real write path (`VolumeBridge::write`), no mount. These
//! establish that dynamic growth is admitted against, and debited from, the one shard budget — the
//! `Store`'s `ShardBudget` — using the server's actual quota construction (`Quota::Dynamic` with the
//! `BudgetGrowth` source, a bounded volume's reservation taken from the budget) and admission
//! (`Quota::admit`, `ShardBudget::reserve`/`grow`). So two dynamic volumes cannot spend the same
//! capacity, a dynamic volume cannot consume a bounded volume's entitlement, a bounded volume's
//! admission accounts for growth already taken, a refused growth leaves credits consistent with what
//! is retained, and a bounded volume keeps its advertised allowance after competing growth is
//! refused. Mounted POSIX and guest conformance are separately pending their environments; this is
//! the server-accounting layer beneath them.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::budget::Reservation;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::error::VfsError;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::{BudgetGrowth, Quota};
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

const PAGE: usize = 4096;
/// A region of 512 pages (2 MiB): a budget small enough that a handful of chunk-sized writes bind it,
/// so the accounting is observable within a test.
const REGION_PAGES: usize = 512;

/// A store whose budget is the small arena's capacity, with no operation headroom (so the whole
/// budget is admittable and the arithmetic in the assertions is the plain capacity).
fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 512,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
    0,
  )
}

/// A dynamic volume with the server's real growth source: growth is admitted against the shard
/// budget on each increment. `max` is the volume's own ceiling; the budget is the shard's.
fn dynamic(store: &mut Store, prefix: u16) -> Volume {
  volume(
    store,
    prefix,
    Quota::Dynamic {
      max: u64::MAX,
      source: Box::new(BudgetGrowth),
      granted: 0,
      denied: 0,
    },
  )
}

fn volume(store: &mut Store, prefix: u16, quota: Quota) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix,
      names: NameEquivalence::Exact,
      quota,
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

fn oid(ino: u64) -> ObjectId {
  ObjectId::new(ino, 0)
}

fn rw_cx(volume: VolumeId) -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      volume,
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

fn vid(prefix: u8) -> VolumeId {
  let mut bytes = [0u8; 16];
  bytes[0] = prefix;
  VolumeId { bytes }
}

/// Writes chunk-sized blocks to a fresh file in `volume` until the write is refused, returning the
/// bytes that actually landed. Each block is a new chunk window, so it grows a dynamic volume by one
/// chunk, admitted against the shard budget; a bounded volume grows within its own limit.
fn grow_until_refused(volume: &mut Volume, store: &mut Store, id: VolumeId) -> u64 {
  let chunk = store.content.chunk_bytes();
  let block = vec![b'x'; chunk];
  let mut bridge = VolumeBridge::new(id, volume, store);
  let cx = rw_cx(id);
  let root = bridge.root(&cx).unwrap();
  let (attr, _fh) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  let file = attr.ino;
  let mut written = 0u64;
  loop {
    match bridge.write(oid(file), &cx, written, &block) {
      Ok(n) => written += u64::from(n),
      Err(VfsError::NoSpace) => break,
      Err(e) => panic!("unexpected write error: {e:?}"),
    }
    assert!(written <= u64::try_from(REGION_PAGES).unwrap() * u64::try_from(PAGE).unwrap());
  }
  written
}

/// History 1: create dynamic A, then reserve bounded B — A cannot consume B's entitlement. A's
/// growth is admitted against the *live* budget, so once B has reserved its share A can only grow
/// into what is left, never into B's committed reservation. (The old private per-volume ceiling,
/// fixed at A's creation before B reserved, would have let A grow into B's space.)
#[test]
fn a_dynamic_volume_cannot_consume_a_bounded_volumes_entitlement() {
  let mut store = store();
  let mut a = dynamic(&mut store, 1);
  let capacity = store.budget.capacity();
  let b_reservation = capacity / 4 * 3;
  store
    .budget
    .reserve(b_reservation)
    .expect("B reserves its entitlement");
  let remaining = store.budget.admittable();

  let grown = grow_until_refused(&mut a, &mut store, vid(1));

  assert!(
    grown <= remaining,
    "A grew {grown} but only {remaining} was unpromised — it consumed B's entitlement"
  );
  assert!(
    store.budget.committed() <= capacity,
    "the budget never over-commits its capacity"
  );
  assert!(
    grown > 0,
    "A did grow into the unpromised remainder (non-vacuous)"
  );
}

/// History 2: grow dynamic A, then request bounded B — B's admission accounts for A's consumption.
/// A reservation for more than the capacity A left is refused; the exact remainder is admitted.
#[test]
fn a_bounded_admission_accounts_for_a_dynamic_volumes_growth() {
  let mut store = store();
  let mut a = dynamic(&mut store, 1);
  let capacity = store.budget.capacity();
  let grown = grow_until_refused(&mut a, &mut store, vid(1));
  assert!(grown > 0, "A grew (non-vacuous)");
  let remaining = store.budget.admittable();
  assert_eq!(
    remaining,
    capacity - store.budget.committed(),
    "with no headroom, the remainder is capacity minus what A holds"
  );

  // A bounded volume asking for more than A left is refused; B's admission sees A's growth.
  assert!(
    store
      .budget
      .reserve(remaining + u64::try_from(PAGE).unwrap())
      .is_err(),
    "B cannot reserve past what A already took"
  );
  // The exact remainder is admitted.
  store
    .budget
    .reserve(remaining)
    .expect("B reserves exactly the capacity A left");
  assert_eq!(
    store.budget.committed(),
    capacity,
    "A's growth plus B's reservation is the whole capacity"
  );
}

/// Creates one file in `volume` and returns its inode number.
fn create_file(volume: &mut Volume, store: &mut Store, id: VolumeId) -> u64 {
  let mut bridge = VolumeBridge::new(id, volume, store);
  let cx = rw_cx(id);
  let root = bridge.root(&cx).unwrap();
  let (attr, _fh) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  attr.ino
}

/// Writes one chunk-sized block at `offset` into `file`, returning the write result (so a caller can
/// see a `NoSpace` refusal rather than unwrap it).
fn write_chunk(
  volume: &mut Volume,
  store: &mut Store,
  id: VolumeId,
  file: u64,
  offset: u64,
) -> Result<u32, VfsError> {
  let block = vec![b'x'; store.content.chunk_bytes()];
  let mut bridge = VolumeBridge::new(id, volume, store);
  let cx = rw_cx(id);
  bridge.write(oid(file), &cx, offset, &block)
}

/// History 3: alternate growth between two dynamic volumes — they cannot spend the same capacity.
/// Both draw from the one shard budget on each chunk, so their combined growth never exceeds the
/// capacity, and the budget's committed bytes are exactly what the two together hold.
#[test]
fn two_dynamic_volumes_cannot_spend_the_same_capacity() {
  let mut store = store();
  let mut a = dynamic(&mut store, 1);
  let mut b = dynamic(&mut store, 2);
  let capacity = store.budget.capacity();
  let fa = create_file(&mut a, &mut store, vid(1));
  let fb = create_file(&mut b, &mut store, vid(2));

  let (mut off_a, mut off_b) = (0u64, 0u64);
  let (mut a_done, mut b_done) = (false, false);
  while !(a_done && b_done) {
    if !a_done {
      match write_chunk(&mut a, &mut store, vid(1), fa, off_a) {
        Ok(n) => off_a += u64::from(n),
        Err(VfsError::NoSpace) => a_done = true,
        Err(e) => panic!("{e:?}"),
      }
    }
    if !b_done {
      match write_chunk(&mut b, &mut store, vid(2), fb, off_b) {
        Ok(n) => off_b += u64::from(n),
        Err(VfsError::NoSpace) => b_done = true,
        Err(e) => panic!("{e:?}"),
      }
    }
  }

  let total = off_a + off_b;
  assert!(
    total <= capacity,
    "A ({off_a}) and B ({off_b}) together spent {total} of {capacity} — a double-spend"
  );
  assert_eq!(
    store.budget.committed(),
    total,
    "the budget's committed bytes are exactly the two volumes' combined growth"
  );
  assert!(off_a > 0 && off_b > 0, "both volumes grew (non-vacuous)");
}

/// History 4: a refused growth leaves the budget consistent — the byte that could not be admitted
/// changes nothing, so the committed total equals exactly what the volume retains, and releasing the
/// hold (teardown) returns it whole.
#[test]
fn a_refused_growth_leaves_the_budget_consistent_with_what_is_retained() {
  let mut store = store();
  let mut a = dynamic(&mut store, 1);
  let grown = grow_until_refused(&mut a, &mut store, vid(1));
  assert!(grown > 0);
  assert_eq!(
    store.budget.committed(),
    grown,
    "the committed total is exactly what A retains — the refused write debited nothing"
  );
  assert_eq!(
    a.budget_hold(),
    grown,
    "the volume's hold matches the budget's committed"
  );
  // Teardown returns the hold whole (accounting through cancellation/teardown).
  store.budget.release(Reservation {
    bytes: a.budget_hold(),
  });
  assert_eq!(
    store.budget.committed(),
    0,
    "releasing the hold returns every credit"
  );
}

/// History 5: after competing growth is refused, the bounded volume can still use its advertised
/// allowance. B reserves half; A grows into the other half and is then refused; B writes its whole
/// allowance and every write lands, because the budget kept A out of B's reserved share (so the arena
/// physically has room for B).
#[test]
fn a_bounded_volume_keeps_its_allowance_after_competing_growth_is_refused() {
  let mut store = store();
  let capacity = store.budget.capacity();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  // B reserves half the capacity, rounded down to whole chunks so its allowance is writable in full.
  let b_allowance = (capacity / 2) / chunk * chunk;
  store
    .budget
    .reserve(b_allowance)
    .expect("B reserves half the capacity");

  // A grows into the other half and is then refused.
  let mut a = dynamic(&mut store, 1);
  let a_grew = grow_until_refused(&mut a, &mut store, vid(1));
  assert!(a_grew > 0, "A grew into the unpromised half");
  assert!(
    store.budget.admittable() < chunk,
    "the unpromised capacity is spent"
  );

  // B writes its whole allowance; every chunk lands (its reserved share is physically there).
  let mut b = volume(&mut store, 2, Quota::Bounded { limit: b_allowance });
  let block = vec![b'b'; usize::try_from(chunk).unwrap()];
  let mut bridge = VolumeBridge::new(vid(2), &mut b, &mut store);
  let cx = rw_cx(vid(2));
  let root = bridge.root(&cx).unwrap();
  let (attr, _) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  let file = attr.ino;
  let mut written = 0u64;
  while written + chunk <= b_allowance {
    let n = bridge
      .write(oid(file), &cx, written, &block)
      .expect("B writes within its reserved allowance even though A exhausted the rest");
    written += u64::from(n);
  }
  assert_eq!(
    written, b_allowance,
    "B used its whole advertised allowance"
  );
}
