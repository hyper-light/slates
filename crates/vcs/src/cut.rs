//! A giant repository's index cut into ranges by its objects' ids (A-724 piece 9; §4.17.5 "A giant repository's metadata
//! across partitions").
//!
//! **Why.** A repository's index (the tool's id to the object's kind, length and content identity, A-581) lives on its
//! owner partition, and its rows sum into that partition's memory and snapshot. git/git's 424,851 objects are about 17
//! MiB of rows; the Linux kernel's ten million about 410 MiB, near a 746,962,928-byte slot on the Studio; Google's
//! monorepo, about a billion files [A: Potvin and Levenberg, CACM 2016], tens of gigabytes. No one partition holds that.
//!
//! **The cut.** The index is cut into `2^b` ranges by the leading `b` bits of each object's id ([`range_of`]): the
//! tool's ids are cryptographic hashes (git's SHA-1 or SHA-256, Mercurial's SHA-1), so the ranges fill evenly. `b` is
//! one small integer the repository's record keeps. Each range is held where the server's placement puts it (rendezvous
//! over the repository's family and the range, never a catalog), so a look-up routes by the id in one hop.
//!
//! **`b`, derived** ([`bits_for`]): the least `b` at which the index's measured bytes, spread evenly, fit the room one
//! partition gives an index, `ceil(log2(index bytes / room))`, at most the id's own bits. git/git derives `b = 0` against
//! any room near a partition's (one partition, nothing routed, R9); the Linux kernel's 410 MiB is `b = 0` against a whole
//! 746,962,928-byte slot and `b = 2` against a quarter of one.
//!
//! **A split** ([`split_due`], [`child_of`]). Evenly is a rate, not a promise: when any one range's measured bytes pass
//! the room, `b` grows by one and every range splits on its next bit, each row going to exactly one of the two children
//! (`2r` or `2r + 1`), so a split moves half of each range and no row twice. Extendible hashing's directory doubling,
//! kept as one integer [A: Fagin, Nievergelt, Pippenger and Strong, "Extendible Hashing", ACM TODS 4(3), 1979].
//!
//! **Hostile input.** An id shorter than the bits asked reads its missing bits as zero; bits past 32 are refused by
//! [`range_of`] (a range is a `u32`); every result is a total function, never a panic.

/// The most bits a cut takes: a range is named by a `u32`. Derived: `u32::BITS`.
pub const MOST_BITS: u8 = 32;

/// The range of the `bits`-bit cut that holds the object named `id` (module docs): `id`'s leading `bits` bits, the
/// most significant first; none for a cut of more than [`MOST_BITS`].
pub fn range_of(id: &[u8], bits: u8) -> Option<u32> {
  if bits > MOST_BITS {
    return None;
  }
  let mut word = [0u8; 4];
  for (slot, byte) in word.iter_mut().zip(id) {
    *slot = *byte;
  }
  let leading = u32::from_be_bytes(word);
  Some(
    leading
      .checked_shr(u32::from(MOST_BITS.saturating_sub(bits)))
      .unwrap_or(0),
  )
}

/// The least cut at which an index of `index_bytes`, spread evenly over its ranges, fits `room` bytes a range (module
/// docs, "`b`, derived"): `ceil(log2(index_bytes / room))`, zero when it fits whole, at most [`MOST_BITS`]; a room of zero
/// takes the most.
pub fn bits_for(index_bytes: u64, room: u64) -> u8 {
  if room == 0 {
    return if index_bytes == 0 { 0 } else { MOST_BITS };
  }
  let mut bits = 0u8;
  while bits < MOST_BITS {
    let ranges = 1u64.checked_shl(u32::from(bits)).unwrap_or(u64::MAX);
    if index_bytes.div_ceil(ranges) <= room {
      return bits;
    }
    bits = bits.saturating_add(1);
  }
  MOST_BITS
}

/// Whether the cut is due to split (module docs, "A split"): some range's measured bytes, of `ranges`, pass `room`, and
/// the cut has a bit left.
pub fn split_due(ranges: impl IntoIterator<Item = u64>, room: u64, bits: u8) -> bool {
  bits < MOST_BITS && ranges.into_iter().any(|bytes| bytes > room)
}

/// The child range, at the `bits + 1`-bit cut, that the object named `id` moves to when range `range` of the `bits`-bit
/// cut splits: `2 * range` or `2 * range + 1` by `id`'s next bit; none past [`MOST_BITS`].
pub fn child_of(id: &[u8], bits: u8) -> Option<u32> {
  range_of(id, bits.checked_add(1)?)
}

#[cfg(test)]
mod tests {
  use super::{MOST_BITS, bits_for, child_of, range_of, split_due};
  use proptest::prelude::*;
  use std::collections::BTreeMap;

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(300)))]

    /// A-724 piece 9 (§4.17.5). Do: cut any set of ids at any number of bits, then split. Expect: each id in the range
    /// its leading bits name and in no other, every range below `2^bits`; on a split each id moves to `2r` or `2r + 1` of
    /// its range `r`, the two children holding exactly the parent's ids between them; ids that share their first bits
    /// share a range.
    #[test]
    fn every_id_is_in_one_range_and_a_split_moves_it_to_one_of_its_two_children(
      ids in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 20..=32), 1..200),
      bits in 0u8..=24,
    ) {
      let mut by_range: BTreeMap<u32, Vec<&Vec<u8>>> = BTreeMap::new();
      for id in &ids {
        let range = range_of(id, bits).unwrap();
        prop_assert!(u64::from(range) < 1u64 << bits);
        by_range.entry(range).or_default().push(id);
      }
      for (range, members) in &by_range {
        let mut children: BTreeMap<u32, usize> = BTreeMap::new();
        for id in members {
          let child = child_of(id, bits).unwrap();
          prop_assert!(child == range * 2 || child == range * 2 + 1, "{child} not a child of {range}");
          *children.entry(child).or_default() += 1;
        }
        prop_assert_eq!(children.values().sum::<usize>(), members.len());
      }
      if let (Some(first), Some(second)) = (ids.first(), ids.get(1)) {
        let shared = first.iter().zip(second).take_while(|(a, b)| a == b).count();
        if shared * 8 >= usize::from(bits) {
          prop_assert_eq!(range_of(first, bits), range_of(second, bits));
        }
      }
    }

    /// A-724 piece 9. Do: derive the cut for any index size and room. Expect: the index spread evenly over its ranges
    /// fits the room, and one bit fewer would not (least), unless the bits ran out.
    #[test]
    fn the_derived_cut_is_the_least_that_fits(index in 0u64..1u64 << 40, room in 1u64..1u64 << 32) {
      let bits = bits_for(index, room);
      if bits < MOST_BITS {
        prop_assert!(index.div_ceil(1u64 << bits) <= room);
      }
      if bits > 0 && bits < MOST_BITS {
        prop_assert!(index.div_ceil(1u64 << (bits - 1)) > room);
      }
    }
  }

  /// A-724 piece 9, with the figures that motivate it. Do: derive the cut for git/git's, the Linux kernel's and a
  /// billion-object index at 41 bytes a row against a room of a quarter of the Studio's slot. Expect: git/git and the
  /// Linux kernel on one partition (no bits), the billion-object index cut, each range within the room.
  #[test]
  fn git_and_the_kernel_stay_whole_and_a_monorepo_is_cut() {
    let row = 41u64;
    let room = 746_962_928u64 / 4;
    assert_eq!(bits_for(424_851 * row, room), 0);
    assert_eq!(bits_for(10_000_000 * row, room), 2);
    let monorepo = bits_for(1_000_000_000 * row, room);
    assert!(monorepo >= 8, "{monorepo} bits");
    assert!((1_000_000_000 * row).div_ceil(1 << monorepo) <= room);
  }

  /// A-724 piece 9. Do: ask for a split with every range within the room, then one range past it, then at the most bits;
  /// read hostile ids. Expect: due only when a range passes the room and a bit is left; a short id's missing bits read as
  /// zero; bits past the most refused; a room of zero taking the most bits.
  #[test]
  fn a_split_is_due_only_past_the_room_and_hostile_input_is_total() {
    assert!(!split_due([10, 20, 30], 30, 3));
    assert!(split_due([10, 31, 30], 30, 3));
    assert!(!split_due([10, 31, 30], 30, MOST_BITS));
    assert_eq!(range_of(&[0xff], 12), Some(0xff0));
    assert_eq!(range_of(&[], 8), Some(0));
    assert_eq!(range_of(&[1, 2, 3, 4], 33), None);
    assert_eq!(child_of(&[1, 2, 3, 4], MOST_BITS), None);
    assert_eq!(bits_for(5, 0), MOST_BITS);
    assert_eq!(bits_for(0, 0), 0);
  }
}
