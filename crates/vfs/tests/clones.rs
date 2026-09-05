//! T-1.6 in its Phase 1 form: two clones of one snapshot interleave clone-and-write under a
//! generated schedule; each clone's view is its own and the base (the snapshot and the origin
//! head) is unchanged. Clones live on one shard in Phase 1, so the interleaving is a generated
//! schedule over one thread; the shuttle version over two shards comes with Phase 2 (GAPS §1).

// Test harness code: an unwrap here is a failed test, which is what it should be. proptest's
// strategy types carry `Arc`; the test harness exception of D-8 covers it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_types)]

mod common;

use common::{clone_config, store, volume};
use proptest::prelude::*;
use slates_vfs::volume::Volume;

/// Format: the base bytes both clones start from.
const BASE: &[u8] = b"base bytes of the shared snapshot";

fn names(vol: &Volume, store: &slates_vfs::volume::Store) -> Vec<String> {
  let mut names: Vec<String> = vol
    .readdir(store, vol.root())
    .unwrap()
    .iter()
    .map(|r| r.name.to_string())
    .collect();
  names.sort();
  names
}

proptest! {
  #![proptest_config(ProptestConfig { cases: 200, .. ProptestConfig::default() })]

  /// T-1.6: under any interleaving, each clone sees only its own writes and creates, the
  /// snapshot and the origin head still read the base bytes and list only the base file.
  #[test]
  fn two_clones_interleaved_stay_independent_and_the_base_is_unchanged(
    schedule in prop::collection::vec(any::<bool>(), 1..40)
  ) {
    let mut store = store();
    let mut origin = volume(&mut store, 1 << 24);
    let root = origin.root();
    let f = origin.create_file(&mut store, root, "f", 0o644).unwrap();
    origin.write(&mut store, f, 0, BASE).unwrap();
    let s = origin.snapshot(&mut store).unwrap();
    let mut clones = [
      Volume::clone_of(&store, &mut origin, s, clone_config(8)).unwrap(),
      Volume::clone_of(&store, &mut origin, s, clone_config(9)).unwrap(),
    ];
    let mut expected = [BASE.to_vec(), BASE.to_vec()];
    let mut created: [Vec<String>; 2] = [vec!["f".to_owned()], vec!["f".to_owned()]];
    for (i, who) in schedule.iter().enumerate() {
      let k = usize::from(*who);
      let tag = b"AB"[k];
      let off = u64::try_from(i).unwrap();
      clones[k].write(&mut store, f, off, &[tag]).unwrap();
      if expected[k].len() <= i {
        expected[k].resize(i + 1, 0);
      }
      expected[k][i] = tag;
      let name = format!("c{i}");
      let croot = clones[k].root();
      clones[k].create_file(&mut store, croot, &name, 0o644).unwrap();
      created[k].push(name);
    }
    let mut buf = vec![0u8; 64];
    for k in 0..2 {
      let n = clones[k].read(&store, f, 0, &mut buf).unwrap();
      prop_assert_eq!(&buf[..n], &expected[k][..], "clone {}", k);
      created[k].sort();
      prop_assert_eq!(names(&clones[k], &store), created[k].clone(), "clone {} listing", k);
    }
    let n = origin.read(&store, f, 0, &mut buf).unwrap();
    prop_assert_eq!(&buf[..n], BASE, "the origin head");
    let n = origin.read_in(&store, s, f, 0, &mut buf).unwrap();
    prop_assert_eq!(&buf[..n], BASE, "the snapshot");
    prop_assert_eq!(names(&origin, &store), vec!["f".to_owned()]);
  }
}
