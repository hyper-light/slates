//! The change counter (`Volume::change_version`, served as NFSv4's `change` attribute and in its
//! `change_info4`, A-38): it moves on every change to an object, whatever the wall clock does. A
//! client keeps its cached attributes, data and directory listings for as long as `change` is the
//! value it saw, so a change the counter missed is served stale; a wall clock can repeat a stamp
//! (its resolution, a microsecond on macOS) or step back (a time adjustment), so ctime cannot be it.
//!
//! The oracle runs one generated history twice: once under a clock that steps on every read, where
//! a change always moves ctime, and once under a frozen clock, where ctime never moves. Wherever an
//! object's ctime moved in the first run, its counter must have moved in the second, and no
//! counter ever goes backwards.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;

use common::drive::{apply_volume, head_state};
use common::steps::{Step, step};
use common::store;
use proptest::prelude::*;
use slates_vfs::clock::StepClock;
use slates_vfs::ids::InodeNo;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, Volume, VolumeConfig};

/// Shape: the quota of the test volumes, far above what a generated history writes.
const QUOTA: u64 = 1 << 24;

/// A volume whose wall clock advances by `step` nanoseconds per read (0: frozen).
fn volume_clocked(store: &mut Store, step: u64) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Fold,
      quota: Quota::Bounded { limit: QUOTA },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(1, step)),
    },
  )
  .unwrap()
}

/// Every object in the head, by inode number: its change time and its change counter.
fn objects(vol: &Volume, store: &Store) -> BTreeMap<InodeNo, (i64, u64)> {
  let state = head_state(vol, store);
  let paths = state
    .dir_paths()
    .into_iter()
    .chain(state.files.keys().cloned())
    .chain(state.symlinks.keys().cloned());
  let mut out = BTreeMap::new();
  for path in paths {
    let mut no = vol.root_inode(store).unwrap();
    for component in path.split('/').filter(|part| !part.is_empty()) {
      no = vol.lookup_no(store, no, component).unwrap().inode;
    }
    let ctime = vol.stat(store, no).unwrap().ctime;
    out.insert(no, (ctime, vol.change_version(store, no).unwrap()));
  }
  out
}

/// Applies `step` to the volume, the picks drawn from its head's files.
fn apply(step: &Step, vol: &mut Volume, store: &mut Store) {
  let files = head_state(vol, store).file_paths();
  let _ = apply_volume(step, vol, store, &files);
}

proptest! {
  #![proptest_config(ProptestConfig::with_cases(256))]

  /// A-38: on generated histories, every object whose ctime moves under a stepping clock has its
  /// change counter moved under a frozen clock, at every step; no counter ever decreases.
  #[test]
  fn the_change_counter_moves_wherever_ctime_would(
    history in proptest::collection::vec(step(), 1..48),
  ) {
    let mut stepped_store = store();
    let mut stepped = volume_clocked(&mut stepped_store, 1_000);
    let mut frozen_store = store();
    let mut frozen = volume_clocked(&mut frozen_store, 0);
    for (index, step) in history.iter().enumerate() {
      let clocked_before = objects(&stepped, &stepped_store);
      let counted_before = objects(&frozen, &frozen_store);
      apply(step, &mut stepped, &mut stepped_store);
      apply(step, &mut frozen, &mut frozen_store);
      let clocked_after = objects(&stepped, &stepped_store);
      let counted_after = objects(&frozen, &frozen_store);
      prop_assert_eq!(
        clocked_after.keys().collect::<Vec<_>>(),
        counted_after.keys().collect::<Vec<_>>(),
        "the two runs hold the same objects after step {}", index
      );
      for (no, (ctime, _)) in &clocked_after {
        let (Some((ctime_before, _)), Some((_, counter_before)), Some((_, counter))) = (
          clocked_before.get(no),
          counted_before.get(no),
          counted_after.get(no),
        ) else {
          continue;
        };
        prop_assert!(counter >= counter_before, "inode {:?}'s counter went back at step {} ({:?})", no, index, step);
        if ctime != ctime_before {
          prop_assert!(
            counter > counter_before,
            "inode {:?} changed at step {} ({:?}) but its counter stayed {}",
            no, index, step, counter
          );
        }
      }
    }
  }
}
