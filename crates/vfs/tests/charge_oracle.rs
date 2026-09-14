//! The §4.2 charge oracle (GAP-A9-1, AC-2.11): generated histories of writes, truncates, edits,
//! unlinks, snapshots and snapshot destroys over one volume, on a store that already holds a
//! neighbour's reservation, checked after every step against what the arena and the ledger must
//! say. The identities are the design's, not the implementation's: the arena holds exactly the
//! bytes the head is charged plus the bytes its snapshots retain (allocator rounding included);
//! the ledger's committed bytes are exactly the reservations plus the retained bytes and never
//! exceed the capacity; the neighbour's whole entitlement stays physically backed; a refused step
//! changes nothing; every admitted byte reads back. Unlike `model.rs`, these histories destroy
//! snapshots in every order, which is where a chunk freed twice or too early shows.
// Test harness code: an unwrap here is a failed test, which is what it should be; proptest's
// strategy union carries an `Arc` (a test harness, the third D-8 exception, as in `model.rs`).
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::disallowed_types
)]

mod common;

use common::{store, volume};
use proptest::prelude::*;
use slates_vfs::error::VfsError;
use slates_vfs::ids::{InodeNo, SnapshotId};
use slates_vfs::volume::{Store, Volume};

/// Shape: the files a history plays on.
const FILES: usize = 3;
/// Shape: the most live snapshots a history keeps (a further `Snapshot` is a no-op), so a history
/// exercises destroys in every order without growing the deadlists past what a case can walk.
const LIVE_SNAPSHOTS: usize = 3;
/// Shape: the widest write, in pages: three chunks (48 pages on a 4 KiB page), so a write crosses
/// windows and reopens several at once.
const WRITE_PAGES: u32 = 48;
/// Shape: the furthest window a write starts in, so a file spans at most a handful of windows.
const WINDOWS: u8 = 4;

#[derive(Clone, Debug)]
enum Step {
  /// A write of `pages` pages starting `at` pages into window `window`.
  Write {
    file: u8,
    window: u8,
    at: u8,
    pages: u32,
    fill: u8,
  },
  /// A truncate to `pages` pages plus `extra` bytes.
  Truncate {
    file: u8,
    pages: u8,
    extra: u16,
  },
  Unlink {
    file: u8,
  },
  Snapshot,
  DestroySnapshot {
    pick: u8,
  },
  /// A splice: `delete` bytes replaced by `insert` bytes, `at` pages plus `extra` bytes in.
  Edit {
    file: u8,
    at: u8,
    extra: u16,
    delete: u16,
    insert: u16,
    fill: u8,
  },
}

fn step() -> impl Strategy<Value = Step> {
  let file = 0..u8::try_from(FILES).unwrap();
  prop_oneof![
    5 => (file.clone(), 0..WINDOWS, 0..16u8, 1..=WRITE_PAGES, any::<u8>()).prop_map(
      |(file, window, at, pages, fill)| Step::Write {
        file,
        window,
        at,
        pages,
        fill
      }
    ),
    2 => (file.clone(), 0..(WINDOWS * 16), any::<u16>()).prop_map(|(file, pages, extra)| {
      Step::Truncate { file, pages, extra }
    }),
    1 => file.clone().prop_map(|file| Step::Unlink { file }),
    2 => Just(Step::Snapshot),
    2 => any::<u8>().prop_map(|pick| Step::DestroySnapshot { pick }),
    2 => (
      file,
      0..(WINDOWS * 16),
      any::<u16>(),
      any::<u16>(),
      any::<u16>(),
      any::<u8>()
    )
      .prop_map(|(file, at, extra, delete, insert, fill)| Step::Edit {
        file,
        at,
        extra,
        delete,
        insert,
        fill
      }),
  ]
}

/// The files of one history: their inode numbers (once created) and a byte shadow of each.
struct Files {
  names: [&'static str; FILES],
  nos: [Option<InodeNo>; FILES],
  bytes: [Vec<u8>; FILES],
}

/// Everything a refused step must leave alone, and what the identities are checked over.
#[derive(Debug, PartialEq, Eq)]
struct Observation {
  accounting: slates_vfs::quota::Accounting,
  allocated: usize,
  chunks: usize,
  committed: u64,
  retained: u64,
  contents: Vec<Option<Vec<u8>>>,
}

fn observe(store: &Store, vol: &Volume, files: &Files) -> Observation {
  let contents = files
    .nos
    .iter()
    .map(|no| {
      no.map(|no| {
        let size = usize::try_from(vol.stat(store, no).unwrap().size).unwrap();
        let mut back = vec![0u8; size];
        let read = vol.read(store, no, 0, &mut back).unwrap();
        back.truncate(read);
        back
      })
    })
    .collect();
  Observation {
    accounting: vol.accounting(),
    allocated: store.content.allocated_bytes(),
    chunks: store.content.chunks(),
    committed: store.budget.committed(),
    retained: vol.retained_bytes(store),
    contents,
  }
}

/// Applies one step to the volume, and to the shadow when the volume accepted it.
fn apply(
  step: &Step,
  vol: &mut Volume,
  store: &mut Store,
  files: &mut Files,
  snapshots: &mut Vec<SnapshotId>,
) -> Result<(), VfsError> {
  let page = u64::try_from(store.content.page()).unwrap();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let root = vol.root_inode(store).unwrap();
  match step {
    Step::Write {
      file,
      window,
      at,
      pages,
      fill,
    } => {
      let index = usize::from(*file);
      let no = match files.nos[index] {
        Some(no) => no,
        None => {
          let no = vol.create_file_no(store, root, files.names[index], 0o644)?;
          files.nos[index] = Some(no);
          no
        }
      };
      let off = u64::from(*window) * chunk + u64::from(*at) * page;
      let bytes = vec![*fill; usize::try_from(u64::from(*pages) * page).unwrap()];
      vol.write(store, no, off, &bytes)?;
      let shadow = &mut files.bytes[index];
      let end = usize::try_from(off).unwrap() + bytes.len();
      if shadow.len() < end {
        shadow.resize(end, 0);
      }
      shadow[usize::try_from(off).unwrap()..end].copy_from_slice(&bytes);
      Ok(())
    }
    Step::Truncate { file, pages, extra } => {
      let index = usize::from(*file);
      let no = files.nos[index].ok_or(VfsError::NotFound)?;
      let len = u64::from(*pages) * page + u64::from(*extra);
      vol.truncate(store, no, len)?;
      files.bytes[index].resize(usize::try_from(len).unwrap(), 0);
      Ok(())
    }
    Step::Unlink { file } => {
      let index = usize::from(*file);
      files.nos[index].ok_or(VfsError::NotFound)?;
      vol.unlink_no(store, root, files.names[index])?;
      files.nos[index] = None;
      files.bytes[index].clear();
      Ok(())
    }
    Step::Snapshot => {
      if snapshots.len() < LIVE_SNAPSHOTS {
        snapshots.push(vol.snapshot(store)?);
      }
      Ok(())
    }
    Step::DestroySnapshot { pick } => {
      if snapshots.is_empty() {
        return Ok(());
      }
      let which = usize::from(*pick) % snapshots.len();
      let id = snapshots.remove(which);
      vol.destroy_snapshot(store, id)
    }
    Step::Edit {
      file,
      at,
      extra,
      delete,
      insert,
      fill,
    } => {
      let index = usize::from(*file);
      let no = files.nos[index].ok_or(VfsError::NotFound)?;
      let at = u64::from(*at) * page + u64::from(*extra);
      let bytes = vec![*fill; usize::from(*insert)];
      vol.edit(store, no, at, u64::from(*delete), &bytes)?;
      let shadow = &mut files.bytes[index];
      let at = usize::try_from(at).unwrap();
      let delete = usize::from(*delete).min(shadow.len() - at);
      shadow.splice(at..at + delete, bytes);
      Ok(())
    }
  }
}

/// One history: the neighbour reserves three quarters of the shard, the volume a bounded eighth,
/// and the rest is what retention may draw on; every step is checked against the identities.
fn run(steps: Vec<Step>) {
  let mut store = store();
  let capacity = store.budget.capacity();
  let neighbour = capacity / 4 * 3;
  let quota = capacity / 8;
  let promised = store.budget.reserve(neighbour).unwrap();
  let own = store.budget.reserve(quota).unwrap();
  let mut vol = volume(&mut store, quota);
  let mut files = Files {
    names: ["a", "b", "c"],
    nos: [None; FILES],
    bytes: [Vec::new(), Vec::new(), Vec::new()],
  };
  let mut snapshots = Vec::new();
  let mut before = observe(&store, &vol, &files);
  for step in &steps {
    let outcome = apply(step, &mut vol, &mut store, &mut files, &mut snapshots);
    let after = observe(&store, &vol, &files);
    if outcome.is_err() {
      assert_eq!(after, before, "a refused {step:?} changed nothing");
    }
    check_identities(&store, &vol, &files, &after, neighbour + quota, step);
    before = after;
  }
  // The neighbour's whole entitlement lands, whatever the history did.
  let mut other = volume(&mut store, neighbour);
  let other_root = other.root_inode(&store).unwrap();
  let g = other
    .create_file_no(&mut store, other_root, "g", 0o644)
    .unwrap();
  let chunk = store.content.chunk_bytes();
  let block = vec![b'n'; chunk];
  let mut written = 0u64;
  while written + u64::try_from(chunk).unwrap() <= neighbour {
    other
      .write(&mut store, g, written, &block)
      .unwrap_or_else(|e| {
        panic!("the neighbour's window at {written} is within its reservation: {e:?}")
      });
    written += u64::try_from(chunk).unwrap();
  }
  store.budget.release(own);
  store.budget.release(promised);
}

fn check_identities(
  store: &Store,
  vol: &Volume,
  files: &Files,
  seen: &Observation,
  reserved: u64,
  step: &Step,
) {
  let referenced = seen.accounting.referenced_bytes;
  assert_eq!(
    u64::try_from(seen.allocated).unwrap(),
    referenced + seen.retained,
    "after {step:?}: the arena holds exactly the head's charged bytes plus the retained bytes"
  );
  assert_eq!(
    seen.committed,
    reserved + seen.retained,
    "after {step:?}: committed is the reservations plus the retained bytes"
  );
  assert!(
    seen.committed <= store.budget.capacity(),
    "after {step:?}: never over-committed"
  );
  let free = store.budget.capacity() - u64::try_from(seen.allocated).unwrap();
  assert!(
    free >= reserved - referenced.min(reserved),
    "after {step:?}: every reserved byte not yet used is physically free ({free} free)"
  );
  assert_eq!(
    vol.retention_shortfall_bytes(),
    0,
    "after {step:?}: every retained byte was secured first"
  );
  for (index, content) in seen.contents.iter().enumerate() {
    if let Some(bytes) = content {
      assert_eq!(
        bytes, &files.bytes[index],
        "after {step:?}: file {index} reads back what was written"
      );
    }
  }
}

proptest! {
  #![proptest_config(ProptestConfig { cases: 150, max_shrink_iters: 3000, failure_persistence: None, .. ProptestConfig::default() })]

  /// AC-2.11 / AC-0.10: the charge identities hold on every generated history.
  #[test]
  fn the_charge_identities_hold_on_every_history(steps in prop::collection::vec(step(), 1..40)) {
    run(steps);
  }
}
