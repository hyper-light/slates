//! The block map's arithmetic held to its definition (A-160), at sizes no test file could reach: for a file of `n` data
//! blocks, the data blocks and the indirect blocks together fill exactly the slots `0..slots` once each; every
//! indirect block comes before the blocks it points to; each points to exactly the blocks its level maps; and walking
//! a data block's path through the indirect blocks reaches that data block's slot. Checked at and around every
//! boundary through the triple-indirect level.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use slates_image::layout::{self, Geometry, Indirect, Map, POINTERS, Path};

/// The data-block counts checked: each boundary of the map and its neighbours.
fn boundaries() -> Vec<u64> {
  let p = POINTERS;
  let single = 12;
  let double = single + p;
  let triple = double + p * p;
  vec![
    0,
    1,
    11,
    12,
    13,
    double - 1,
    double,
    double + 1,
    double + p,
    double + p + 1,
    double + 3 * p + 7,
    triple - 1,
    triple,
    triple + 1,
    triple + p + 1,
    triple + p * p + 1,
  ]
}

/// A-160 (the map). Do: for each boundary count, list every data block's slot and every indirect block's slot and
/// children. Expect the slots `0..slots` each exactly once; each indirect block before its children; its children
/// exactly its level's blocks; and each data block's path, followed through the indirect blocks, ending at its slot.
#[test]
fn every_map_fills_its_slots_once_with_each_indirect_block_before_what_it_maps() {
  for n in boundaries() {
    let map = Map::new(n).unwrap();
    let slots = map.slots();
    let mut seen = vec![0u8; usize::try_from(slots).unwrap()];
    for index in 0..n {
      seen[usize::try_from(Map::data_slot(index)).unwrap()] += 1;
    }
    let indirect = map.indirect();
    let mut by_slot: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for block in &indirect {
      let (slot, children) = match block {
        Indirect::Data { slot, first, count } => {
          (*slot, (*first..first + count).map(Map::data_slot).collect())
        }
        Indirect::Blocks { slot, children } => (*slot, children.clone()),
      };
      seen[usize::try_from(slot).unwrap()] += 1;
      assert!(
        children.iter().all(|child| *child > slot),
        "n={n}: an indirect block precedes its children"
      );
      assert!(
        children.len() as u64 <= POINTERS,
        "n={n}: at most one block of pointers"
      );
      by_slot.insert(slot, children);
    }
    assert!(
      seen.iter().all(|count| *count == 1),
      "n={n}: every slot once"
    );
    let tops = map.pointer_slots();
    for index in (0..n)
      .step_by(usize::try_from((n / 4096).max(1)).unwrap())
      .chain(n.checked_sub(1))
    {
      assert_eq!(
        walk(&tops, &by_slot, index),
        Map::data_slot(index),
        "n={n}: data block {index}'s path"
      );
    }
  }
}

/// The slot data block `index`'s path reaches from the inode's top pointers `tops` through the indirect blocks.
fn walk(tops: &[u64], by_slot: &BTreeMap<u64, Vec<u64>>, index: u64) -> u64 {
  let child = |slot: u64, at: u64| by_slot[&slot][usize::try_from(at).unwrap()];
  match layout::path(index).unwrap() {
    Path::Direct(slot) => tops[slot],
    Path::Single(first) => child(tops[12], first),
    Path::Double(first, second) => child(child(tops[13], first), second),
    Path::Triple(first, second, third) => child(child(child(tops[14], first), second), third),
  }
}

/// A-160 (the largest file). Do: ask for the largest file the format maps and counts. Expect it within the 2 TiB
/// `i_blocks` bound (32-bit sectors, no `huge_file`), and one block more refused.
#[test]
fn the_largest_file_is_held_to_the_sector_count() {
  let largest = layout::max_file_bytes();
  let blocks = largest / layout::BLOCK_BYTES;
  assert!(Map::new(blocks).is_ok());
  assert!(Map::new(blocks + 1).is_err());
  assert!(Map::new(blocks).unwrap().slots() * layout::SECTORS_PER_BLOCK <= u64::from(u32::MAX));
  assert!(largest > (1u64 << 40), "past 1 TiB: {largest}");
}

/// A-160 (the geometry). Do: plan file systems from one block group's worth to a few groups, and one whose last group
/// would be too small for its own structures. Expect every group's structures inside it, superblock copies only in
/// groups 0, 1 and powers of 3, 5 and 7, and the too-small last group dropped.
#[test]
fn the_geometry_keeps_every_groups_structures_inside_it() {
  for capacity in [1u64 << 20, 128 << 20, 1 << 30, (1 << 30) + 4096 * 10] {
    let geometry = Geometry::plan(capacity, 100).unwrap();
    for group in 0..geometry.groups {
      let start = geometry.group_start(group);
      let end = start + geometry.group_blocks(group);
      assert!(
        geometry.data_start(group) < end,
        "capacity {capacity}, group {group}: room for data"
      );
      assert!(geometry.block_bitmap(group) >= start);
    }
    assert!(geometry.inodes() >= 100);
  }
  let copies: Vec<u64> = (0..50)
    .filter(|group| Geometry::has_super(*group))
    .collect();
  assert_eq!(copies, [0, 1, 3, 5, 7, 9, 25, 27, 49]);
  // A capacity ten blocks past a group boundary: the last group cannot hold its own bitmaps and table, so it goes.
  let geometry = Geometry::plan((1 << 30) + 4096 * 10, 100).unwrap();
  assert_eq!(
    geometry.blocks % layout::BLOCKS_PER_GROUP,
    0,
    "the sliver dropped"
  );
}

/// A-160 (times and device numbers). Do: encode and decode times at the edges of the 34-bit range and device numbers
/// in both encodings. Expect each back as it was, and a time past either edge refused.
#[test]
fn times_and_device_numbers_round_trip_at_their_edges() {
  use slates_image::Timestamp;
  for secs in [
    -(1i64 << 31),
    -1,
    0,
    1,
    (1 << 31) - 1,
    1 << 31,
    (1 << 32) + 5,
    (1 << 34) - (1 << 31) - 1,
  ] {
    let time = Timestamp {
      secs,
      nanos: 999_999_999,
    };
    let (low, extra) = layout::encode_time(time).unwrap();
    assert_eq!(layout::decode_time(low, extra), Some(time), "secs {secs}");
  }
  assert!(
    layout::encode_time(Timestamp {
      secs: -(1 << 31) - 1,
      nanos: 0
    })
    .is_err()
  );
  assert!(
    layout::encode_time(Timestamp {
      secs: (1 << 34) - (1 << 31),
      nanos: 0
    })
    .is_err()
  );
  for (major, minor) in [(1, 3), (8, 0), (255, 255), (256, 0), (4095, (1 << 20) - 1)] {
    let rdev = layout::new_encode(major, minor).unwrap();
    let (first, second) = layout::encode_rdev(rdev);
    assert_eq!(
      layout::decode_rdev(first, second),
      Some(rdev),
      "{major}:{minor}"
    );
    assert_eq!(
      first == 0,
      major > 255 || minor > 255,
      "the old encoding where it fits"
    );
  }
}
