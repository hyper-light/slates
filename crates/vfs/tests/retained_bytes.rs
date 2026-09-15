//! §4.2 all-cost charging, the retained-content dimension (GAP-A9-1): a chunk a snapshot keeps
//! alive after the head let go of it is physical arena capacity that is neither in the volume's
//! `referenced_bytes` (the head no longer reaches it) nor in a bounded volume's reservation. Left
//! uncharged, snapshot-and-overwrite cycles let one volume hold `(snapshots + 1) × quota` of the
//! shard's arena while the budget still shows `quota` committed, and a neighbour writing within its
//! admitted entitlement finds the arena exhausted — the "retained bytes can defeat the cap" finding.
//! Retention is now charged from unpromised capacity by the operation that causes it (a write's
//! reopen, a truncate's cut, an unlink's last name, an edit's splice), secured before the mutation
//! and refused `NoSpace` with nothing changed when the shard cannot back it, and credited back as
//! the retained chunks are freed. These tests drive a bare store (no server-side reservation), so
//! the budget's committed bytes are the retention charge alone and must equal the drift-free
//! `retained_bytes` at every step.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{clone_config, store, volume};
use slates_vfs::clock::StepClock;
use slates_vfs::error::VfsError;
use slates_vfs::ids::InodeNo;
use slates_vfs::volume::{Store, Volume};

/// A file holding `windows` whole chunk windows of `fill`, written window by window.
fn write_windows(vol: &mut Volume, store: &mut Store, no: InodeNo, windows: u64, fill: u8) {
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let block = vec![fill; usize::try_from(chunk).unwrap()];
  for window in 0..windows {
    vol.write(store, no, window * chunk, &block).unwrap();
  }
}

/// The charge/credit balance: with no reservation in the store, the budget's committed bytes are
/// the retention charge alone and equal the drift-free retained bytes; the retained sub-account
/// agrees; and no deadlist push ever found its bytes unsecured.
fn assert_balanced(store: &Store, vol: &Volume, what: &str) {
  assert_eq!(
    store.budget.committed(),
    vol.retained_bytes(store),
    "{what}: the shard budget's committed bytes are exactly the retained content bytes"
  );
  assert_eq!(
    store.budget.retained(),
    vol.retained_bytes(store),
    "{what}: the retained sub-account agrees"
  );
  assert_eq!(
    vol.retention_shortfall_bytes(),
    0,
    "{what}: every retained byte was secured before it landed"
  );
}

/// T-A9 (byte retention): the retention charge equals the retained content bytes at every step of
/// snapshot-and-overwrite and snapshot destruction.
#[test]
fn the_retention_charge_balances_the_retained_content_bytes() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'1');
  assert_balanced(&store, &vol, "before any snapshot");
  assert_eq!(
    vol.retained_bytes(&store),
    0,
    "no snapshot, nothing retained"
  );

  // A snapshot alone retains nothing; the divergence after it does — one window's chunk.
  let s1 = vol.snapshot(&mut store).unwrap();
  assert_balanced(&store, &vol, "after the snapshot");
  assert_eq!(store.budget.committed(), 0);
  let block = vec![b'2'; usize::try_from(chunk).unwrap()];
  vol.write(&mut store, f, 0, &block).unwrap();
  assert_balanced(&store, &vol, "after window 0 diverged");
  assert_eq!(
    store.budget.committed(),
    chunk,
    "window 0's pre-snapshot chunk is retained and charged at its block length"
  );
  // The second window diverges too.
  vol.write(&mut store, f, chunk, &block).unwrap();
  assert_balanced(&store, &vol, "after window 1 diverged");
  assert_eq!(store.budget.committed(), 2 * chunk);
  // An in-epoch rewrite of window 0 retains nothing new (the chunk it replaces was born after the
  // snapshot, so it is freed at once).
  vol.write(&mut store, f, 0, &block).unwrap();
  assert_balanced(&store, &vol, "after an in-epoch rewrite");
  assert_eq!(store.budget.committed(), 2 * chunk);

  // Destroying the snapshot frees both retained chunks and returns the charge.
  vol.destroy_snapshot(&mut store, s1).unwrap();
  assert_balanced(&store, &vol, "after the snapshot's destroy");
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
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'1');
  vol.snapshot(&mut store).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'2');
  assert_eq!(
    store.budget.committed(),
    2 * chunk,
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
/// anything changes (the refusal counter moves), and B then writes its whole allowance with every
/// chunk landing. Non-vacuous: with retention uncharged, A's third cycle succeeds and B's write
/// meets an exhausted arena.
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

  // Cycles 1 and 2 retain a quarter each — the unpromised half (every window's write lands).
  // Cycle 3 would retain into B's (or A's own) promised space and is refused, whole, at the first
  // window.
  a.snapshot(&mut store).unwrap();
  write_windows(&mut a, &mut store, f, windows, b'1');
  a.snapshot(&mut store).unwrap();
  write_windows(&mut a, &mut store, f, windows, b'2');
  assert_eq!(
    store.budget.admittable(),
    0,
    "the unpromised half is spent on retention"
  );
  a.snapshot(&mut store).unwrap();
  let block = vec![b'x'; usize::try_from(chunk).unwrap()];
  assert_eq!(
    a.write(&mut store, f, 0, &block),
    Err(VfsError::NoSpace),
    "a retention into promised space is refused"
  );
  assert_eq!(
    (
      a.retention_refusals(),
      store.budget.committed(),
      store.budget.retained()
    ),
    (1, 4 * quarter, 2 * quarter),
    "the refusal path fired once and changed nothing: two reservations plus two cycles of retention"
  );

  // B writes its whole allowance: every chunk lands, because retention never spent B's share.
  let mut b = volume(&mut store, quarter);
  let b_root = b.root_inode(&store).unwrap();
  let g = b.create_file_no(&mut store, b_root, "g", 0o644).unwrap();
  write_windows(&mut b, &mut store, g, windows, b'x');
  assert_eq!(
    b.accounting().referenced_bytes,
    quarter,
    "B used its whole advertised allowance"
  );
  // Teardown returns everything.
  store.budget.release(a_reservation);
  store.budget.release(b_reservation);
}

/// T-A9 (byte retention): the last name of a snapshot-pinned file retains its windows, charged at
/// the unlink; when no unpromised capacity remains the unlink is refused `NoSpace` with the name
/// still present (as a full OpenZFS pool refuses a delete whose blocks a snapshot holds), and it
/// succeeds once capacity returns.
#[test]
fn an_unlink_retains_the_pinned_windows_and_is_refused_typed_without_unpromised_capacity() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  let g = vol.create_file_no(&mut store, root, "g", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'f');
  write_windows(&mut vol, &mut store, g, 2, b'g');
  vol.snapshot(&mut store).unwrap();
  vol.unlink_no(&mut store, root, "f").unwrap();
  assert_balanced(&store, &vol, "after f's unlink");
  assert_eq!(
    (
      store.budget.committed(),
      vol.lookup(&store, vol.root(), "f").err()
    ),
    (2 * chunk, Some(VfsError::NotFound)),
    "f's two windows are retained by the snapshot and charged, and its name is gone"
  );

  // Promise every remaining byte away: g's unlink would retain two windows into promised space.
  let promise = store.budget.reserve(store.budget.admittable()).unwrap();
  assert_eq!(
    vol.unlink_no(&mut store, root, "g"),
    Err(VfsError::NoSpace),
    "an unlink that would retain into promised space is refused"
  );
  assert_eq!(
    (
      vol.lookup(&store, vol.root(), "g").is_ok(),
      vol.retention_refusals(),
      store.budget.retained(),
      vol.accounting().referenced_bytes
    ),
    (true, 1, 2 * chunk, 2 * chunk),
    "the refused unlink left g's name in place, counted once, charged nothing, and g's content is still the head's"
  );

  // Capacity returns: the unlink lands and retains g's windows.
  store.budget.release(promise);
  vol.unlink_no(&mut store, root, "g").unwrap();
  assert_balanced(&store, &vol, "after g's unlink");
  assert_eq!(store.budget.committed(), 4 * chunk);
}

/// T-A9 (byte retention): a file unlinked while open is charged at the unlink (the last close
/// cannot refuse) and settled at the reclaim: a write into the orphan meanwhile retains its own
/// window through its own charge, and the reclaim consumes what the unlink secured and returns the
/// surplus, so the ledger balances at every step and nothing is charged twice for long.
#[test]
fn an_open_unlinked_file_is_charged_at_its_unlink_and_settled_at_its_last_close() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'o');
  vol.reference(&store, f).unwrap(); // held open
  vol.snapshot(&mut store).unwrap();
  vol.unlink_no(&mut store, root, "f").unwrap();
  let mut back = vec![0u8; usize::try_from(chunk).unwrap()];
  assert_eq!(
    (
      store.budget.committed(),
      vol.retained_bytes(&store),
      vol.read(&store, f, 0, &mut back).ok()
    ),
    (2 * chunk, 0, Some(usize::try_from(chunk).unwrap())),
    "the unlink secured the orphan's two pinned windows; nothing is on a deadlist yet and the open orphan still serves"
  );

  // A write into the orphan's window 0 reopens it: the old chunk is retained through the write's
  // own charge, so the ledger briefly holds the unlink's two plus this one.
  let block = vec![b'n'; usize::try_from(chunk).unwrap()];
  vol.write(&mut store, f, 0, &block).unwrap();
  assert_eq!(
    (vol.retained_bytes(&store), store.budget.committed()),
    (chunk, 3 * chunk),
    "the reopened window is retained through the write's charge"
  );

  // The last close reclaims the orphan: window 0's new chunk (born after the snapshot) is freed,
  // window 1's old chunk is retained from the unlink's charge, and the surplus returns.
  vol.unreference(&mut store, f).unwrap();
  assert_balanced(&store, &vol, "after the reclaim");
  assert_eq!(
    (
      store.budget.committed(),
      vol.read(&store, f, 0, &mut back).err()
    ),
    (2 * chunk, Some(VfsError::NotFound)),
    "the two pre-snapshot windows are retained, the surplus returned, the orphan gone"
  );
}

/// T-A9 (byte retention): a truncate charges the windows it cuts away, and an edit charges the
/// windows it cuts plus the one it reopens, each balanced against the deadlists.
#[test]
fn a_truncate_and_an_edit_charge_the_windows_they_release() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 3, b't');
  vol.snapshot(&mut store).unwrap();
  vol.truncate(&mut store, f, chunk).unwrap();
  assert_balanced(&store, &vol, "after the truncate");
  assert_eq!(
    store.budget.committed(),
    2 * chunk,
    "the two windows past the cut are retained"
  );
  // An edit inside window 0 keeps the (partly kept) chunk through the cut, then reopens the window
  // for the write: that reopen retains the pre-snapshot chunk.
  vol.edit(&mut store, f, chunk / 2, 0, b"x").unwrap();
  assert_balanced(&store, &vol, "after the edit");
  assert_eq!(store.budget.committed(), 3 * chunk);
  let mut back = vec![0u8; 3];
  vol.read(&store, f, chunk / 2 - 1, &mut back).unwrap();
  assert_eq!(&back, b"txt", "the edit spliced the byte in");
}

/// T-A9 (byte retention, clones): a clone's snapshot retains only what the clone owns. Overwriting
/// an inherited window charges nothing (the origin's pinned snapshot holds that chunk whatever the
/// clone does), while a window the clone wrote itself and then overwrote after its own snapshot is
/// charged — for bytes and for inode versions alike.
#[test]
fn a_clones_snapshot_charges_nothing_for_the_windows_its_origin_owns() {
  let mut store = store();
  let mut origin = volume(&mut store, 1 << 24);
  let root = origin.root_inode(&store).unwrap();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let f = origin.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut origin, &mut store, f, 2, b'i');
  let pinned = origin.snapshot(&mut store).unwrap();
  let mut clone = Volume::clone_of(&store, &mut origin, pinned, clone_config(8)).unwrap();
  let block = vec![b'c'; usize::try_from(chunk).unwrap()];

  // The clone diverges from its inheritance: the inherited version and window are the origin's.
  clone.write(&mut store, f, 0, &block).unwrap();
  assert_retained(
    &store,
    &clone,
    0,
    0,
    "an inherited window and version cost the clone nothing",
  );

  // The clone's own snapshot pins its own window 0 and its own version; overwriting them retains.
  clone.snapshot(&mut store).unwrap();
  clone.write(&mut store, f, 0, &block).unwrap();
  assert_retained(
    &store,
    &clone,
    chunk,
    1,
    "the clone's own window and version are retained and charged",
  );
  // Window 1 is still the inherited one: pinned by the clone's snapshot too, but the origin's.
  clone.write(&mut store, f, chunk, &block).unwrap();
  assert_retained(
    &store,
    &clone,
    chunk,
    1,
    "the inherited window is not charged",
  );
  assert_eq!(
    origin.accounting().referenced_bytes,
    2 * chunk,
    "the origin still reaches both of its windows"
  );
}

/// Both dimensions' retention as the budgets and the volume report it: the committed bytes and
/// slots (retention alone, in these stores) and the volume's own drift-free counts.
fn assert_retained(store: &Store, vol: &Volume, bytes: u64, versions: u64, what: &str) {
  assert_eq!(
    (
      store.budget.committed(),
      vol.retained_bytes(store),
      store.versions.committed(),
      vol.retained_versions()
    ),
    (bytes, bytes, versions, versions),
    "{what}"
  );
}

/// T-A9 (byte retention, recovery): a rebuilt volume re-establishes its retention against the
/// fresh shard's budget — the private copies its recovered snapshot holds and the bytes secured for
/// an open-unlinked orphan — so the ledger balances right after `from_image`, and an orphan's later
/// reclaim consumes what was secured.
#[test]
fn recovery_re_establishes_the_retention_charge_including_an_orphans() {
  let mut source = store();
  let mut vol = volume(&mut source, 1 << 24);
  let root = vol.root_inode(&source).unwrap();
  let chunk = u64::try_from(source.content.chunk_bytes()).unwrap();
  let f = vol.create_file_no(&mut source, root, "f", 0o644).unwrap();
  let g = vol.create_file_no(&mut source, root, "g", 0o644).unwrap();
  write_windows(&mut vol, &mut source, f, 2, b'f');
  write_windows(&mut vol, &mut source, g, 1, b'g');
  vol.snapshot(&mut source).unwrap();
  vol
    .write(
      &mut source,
      f,
      0,
      &vec![b'F'; usize::try_from(chunk).unwrap()],
    )
    .unwrap();
  vol.reference(&source, g).unwrap();
  vol.unlink_no(&mut source, root, "g").unwrap();
  assert_eq!(
    source.budget.committed(),
    2 * chunk,
    "one retained window plus the orphan's secured window"
  );
  let image = vol.to_image(&source, None).unwrap();

  let mut fresh = store();
  let mut recovered = Volume::from_image(
    &mut fresh,
    &image,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  // The recovered snapshot holds private copies of what diverged from the head: f (two windows,
  // its bytes changed) and g (one window — its version diverged when its link count went to zero,
  // so recovery rebuilds it privately where the live volume's two versions shared one chunk). The
  // orphan's own rebuilt window is born at the head's rebuild epoch, after the snapshot, so its
  // reclaim will free it rather than retain it: the re-establishment secures nothing for it.
  assert_eq!(recovered.retained_bytes(&fresh), 3 * chunk);
  assert_eq!(
    fresh.budget.committed(),
    3 * chunk,
    "the retained copies are charged on the fresh shard; the orphan's window will be freed"
  );
  assert_eq!(
    fresh.versions.committed(),
    recovered.retained_versions(),
    "the version retention is re-established too"
  );
  // The orphan's last close (no reference was restored, so this is it) reclaims it: its rebuilt
  // window was born after the snapshot, so it is freed rather than retained, and the secured bytes
  // return as surplus — the ledger balances either way.
  recovered.unreference(&mut fresh, g).unwrap();
  assert_balanced(&fresh, &recovered, "after the recovered orphan's reclaim");
  assert_eq!(fresh.budget.committed(), 3 * chunk);
}

/// T-1.1 for retention: a refused retention leaves nothing changed — not the accounting, not the
/// arena, not the ledger, not the bytes — for a write, a truncate, an edit and an unlink alike; and
/// the same operations land once capacity returns.
#[test]
fn a_refused_retention_leaves_nothing_changed() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'u');
  vol.snapshot(&mut store).unwrap();
  let promise = store.budget.reserve(store.budget.admittable()).unwrap();
  let block = vec![b'v'; usize::try_from(chunk).unwrap()];
  let before = observe(&store, &vol, f);
  let write = vol.write(&mut store, f, 0, &block).map(drop);
  refused_unchanged(&store, &vol, f, &before, write, "write");
  let truncate = vol.truncate(&mut store, f, 0);
  refused_unchanged(&store, &vol, f, &before, truncate, "truncate");
  let edit = vol.edit(&mut store, f, 0, 0, b"x");
  refused_unchanged(&store, &vol, f, &before, edit, "edit");
  let unlink = vol.unlink_no(&mut store, root, "f");
  refused_unchanged(&store, &vol, f, &before, unlink, "unlink");
  assert_eq!(
    vol.retention_refusals(),
    4,
    "each refusal was the retention's"
  );

  store.budget.release(promise);
  vol.write(&mut store, f, 0, &block).unwrap();
  assert_balanced(&store, &vol, "after capacity returned");
  assert_eq!(store.budget.committed(), chunk);
}

/// Everything a refused operation must leave alone: the accounting, the arena, the chunk records,
/// the ledger, the retained bytes, and the file's own bytes.
type Observation = (
  slates_vfs::quota::Accounting,
  usize,
  usize,
  u64,
  u64,
  usize,
  Vec<u8>,
);

fn observe(store: &Store, vol: &Volume, f: InodeNo) -> Observation {
  let mut back = vec![0u8; 2 * store.content.chunk_bytes()];
  let read = vol.read(store, f, 0, &mut back).unwrap();
  (
    vol.accounting(),
    store.content.allocated_bytes(),
    store.content.chunks(),
    store.budget.committed(),
    vol.retained_bytes(store),
    read,
    back,
  )
}

/// `outcome` was the retention refusal, and the volume and store look exactly as `before`.
fn refused_unchanged(
  store: &Store,
  vol: &Volume,
  f: InodeNo,
  before: &Observation,
  outcome: Result<(), VfsError>,
  what: &str,
) {
  assert_eq!(
    outcome,
    Err(VfsError::NoSpace),
    "the {what} is refused typed"
  );
  assert_eq!(
    &observe(store, vol, f),
    before,
    "the refused {what} changed nothing"
  );
}
