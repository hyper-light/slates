//! Which shard decodes a push (A-724; main's ruling of 2026-11-20: A-722 builds a push's fork where it is decoded, and
//! w-balance supplies the policy that chooses the shard, by measured load; §4.17.5 "Giant repositories").
//!
//! **Why.** A push's intake (its pack read, every object hashed and checked) is most of its CPU: SHA-1 over objects
//! topped the daemon's samples at R = 100 (BENCHMARKS.md, "Many repositories on one daemon, held to their fair share").
//! Run on the repository's owner shard, a hot repository's nineteen pushes ran one after another there (10.2 s against
//! 1.4 s spread over the shards). Only the landing must run on the owner; the intake may run on any shard.
//!
//! **The policy: the shorter of two queues** [A: Mitzenmacher, "The Power of Two Choices in Randomized Load Balancing",
//! IEEE TPDS 12(10), 2001]. Two candidates: the shard the push's connection arrived on (no move), and the shard
//! rendezvous ranks first for the push's own key among the node's shards (`slates_db::register::rendezvous_ranked_by_key`,
//! the one weight every placement ranks by): a stable pseudo-random choice that spreads one repository's pushes. The
//! push is decoded on whichever has fewer intakes decoding now; a tie keeps it where it arrived. With one random choice
//! the busiest shard's queue grows as `ln n / ln ln n`; with two, as `ln ln n / ln 2` (the paper's Theorem 1).
//!
//! **The measure.** Each shard counts the intakes decoding on it ([`Decoding`], a guard: counted from its making to its
//! drop, however the intake ends) in one word of its own on its own cache line, written by its shard alone and read by
//! the others with relaxed loads: a statistic, never a lock or a message on the path (CLAUDE.md §2 item 1; "Hot atomics
//! are cache-line padded"). One word a registry slot, as the doorbell flags are (`crate::daemon`'s `DOORBELL_RANG`), so
//! several daemons in one process never share one.

use std::sync::atomic::{AtomicU64, Ordering};

use slates_db::register::rendezvous_ranked_by_key;

/// One shard's count of the intakes decoding on it, on its own cache line (module docs, "The measure").
#[repr(align(128))]
struct DecodingCount(AtomicU64);

/// Every registry slot's count (module docs): indexed by the runtime's shard id.
static DECODING: [DecodingCount; slates_rt::registry::MAX_SHARDS] =
  [const { DecodingCount(AtomicU64::new(0)) }; slates_rt::registry::MAX_SHARDS];

/// The intakes decoding on runtime shard `shard` now: zero for a shard past the registry.
// Owed caller: A-722's intake off the owner shard (w-git, main's ruling of 2026-11-20) reads it through
// [`intake_shard`]; until it lands only the tests read it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn decoding_on(shard: u16) -> u64 {
  DECODING
    .get(usize::from(shard))
    .map_or(0, |count| count.0.load(Ordering::Relaxed))
}

/// An intake decoding on a shard, counted there until it drops (module docs, "The measure"): a git push holds one from
/// its start on the shard it is decoded on (today its repository's owner, `crate::git_push::ReceivePack`).
#[derive(Debug)]
pub(crate) struct Decoding {
  shard: u16,
}

impl Decoding {
  /// Counts an intake decoding on runtime shard `shard`, its own (made on that shard's thread).
  pub(crate) fn begin(shard: u16) -> Decoding {
    if let Some(count) = DECODING.get(usize::from(shard)) {
      count.0.fetch_add(1, Ordering::Relaxed);
    }
    Decoding { shard }
  }
}

impl Drop for Decoding {
  fn drop(&mut self) {
    if let Some(count) = DECODING.get(usize::from(self.shard)) {
      // A drop only follows its own begin, so the count never goes below zero; saturate all the same.
      let _ = count
        .0
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
          Some(held.saturating_sub(1))
        });
    }
  }
}

/// The shard of `shards` a push keyed `key` that arrived on `arrived` is decoded on (module docs): the arrival shard or
/// the one rendezvous ranks first for `key`, whichever `load` says has fewer intakes decoding, the arrival shard on a
/// tie. `load` is [`decoding_on`] in the daemon; a test passes its own.
// Owed caller: A-722's intake off the owner shard (w-git, main's ruling of 2026-11-20); until it lands only the tests
// call it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn intake_shard(
  arrived: u16,
  key: &[u8],
  shards: &[u16],
  load: impl Fn(u16) -> u64,
) -> u16 {
  let members: Vec<u64> = shards.iter().map(|shard| u64::from(*shard)).collect();
  let other = rendezvous_ranked_by_key(&members, key)
    .first()
    .and_then(|member| u16::try_from(*member).ok())
    .unwrap_or(arrived);
  if load(other) < load(arrived) {
    other
  } else {
    arrived
  }
}

/// The partition of `partitions` that holds range `range` of a repository's index cut (A-724 piece 9, §4.17.5 "A
/// giant repository's metadata across partitions", `slates_vcs::cut`): the one rendezvous ranks first for the repository's
/// family key and the range (`family` is the family's line key, so every repository of the family ranks one way), by the
/// one weight every placement ranks by. No catalog: any shard computes it from the repository's id, the range and the
/// partitions it knows. None for no partitions.
// Owed caller: piece 9's index ranges on their partitions (the next part); until then only the tests call it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn range_partition(family: &[u8], range: u32, partitions: &[u16]) -> Option<u16> {
  let mut key = Vec::with_capacity(family.len().saturating_add(core::mem::size_of::<u32>()));
  key.extend_from_slice(family);
  key.extend_from_slice(&range.to_be_bytes());
  let members: Vec<u64> = partitions
    .iter()
    .map(|partition| u64::from(*partition))
    .collect();
  rendezvous_ranked_by_key(&members, &key)
    .first()
    .and_then(|member| u16::try_from(*member).ok())
}

/// The partition that holds the index row of the object named `object` in `record`'s repository (A-724 piece 9, §4.17.5
/// "A giant repository's metadata across partitions"): the repository's own partition while its cut is zero (the whole
/// index on its owner, R9), else the partition of `partitions` that holds the object's range (`slates_vcs::cut::range_of`
/// of its id at the record's `index_bits`, placed by [`range_partition`] over the repository's family line key). Every
/// shard computes it from the record and the id alone: no catalog (§2 ban 12). None for no partitions.
// Owed caller: the rows' routing of A-724 piece 9 (with record v3, after A-727 a3); until then only the tests call it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn index_partition(
  record: &slates_db::catalog::RepositoryRecord,
  object: &[u8],
  partitions: &[u16],
) -> Option<u16> {
  if record.index_bits == 0 {
    return Some(record.id.partition());
  }
  let range = slates_vcs::cut::range_of(object, record.index_bits)?;
  let family = slates_db::register::ObjectId(record.id.bytes).line_key();
  range_partition(&family.0, range, partitions)
}

/// The cut a repository's index should have (A-724 piece 9, `slates_vcs::cut::bits_for`): the least at which its
/// `index_bytes`, spread evenly over its ranges, fit `room` a range, one bit from `current` at a time toward it (a split
/// or a merge moves each row once), or none when it is where it should be.
// Owed caller: the rows' routing of A-724 piece 9; until then only the tests call it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn index_cut_step(current: u8, index_bytes: u64, room: u64) -> Option<u8> {
  let wanted = slates_vcs::cut::bits_for(index_bytes, room);
  match wanted.cmp(&current) {
    core::cmp::Ordering::Greater => current.checked_add(1),
    core::cmp::Ordering::Less => current.checked_sub(1),
    core::cmp::Ordering::Equal => None,
  }
}

#[cfg(test)]
mod tests {
  use super::{Decoding, decoding_on, intake_shard};
  use std::cell::RefCell;

  /// A-724 (main's ruling: the placement policy A-722 calls). Do: place a hot repository's pushes, all arriving on one
  /// shard, one after another while each stays decoding, over 23 shards; then the same by one random choice alone.
  /// Expect: with two choices, no shard holds more than two of its pushes' decodes at once for 19 pushes (the paper's
  /// `ln ln n / ln 2` bound, about 2.4 at 23 shards, against `ln n / ln ln n`, about 2.7, for one), and the arrival shard
  /// never holds more than its share; with one choice (the rendezvous pick alone) the busiest shard holds more.
  #[test]
  fn a_hot_repositorys_pushes_spread_over_the_shards_by_the_shorter_of_two_queues() {
    let shards: Vec<u16> = (0..23).collect();
    let arrived = 0;
    let held = RefCell::new(vec![0u64; shards.len()]);
    for push in 0..19u32 {
      let key = format!("monorepo push {push}");
      let chosen = intake_shard(arrived, key.as_bytes(), &shards, |shard| {
        held.borrow()[usize::from(shard)]
      });
      held.borrow_mut()[usize::from(chosen)] += 1;
    }
    let two = held.borrow().iter().copied().max().unwrap();
    assert!(
      two <= 2,
      "two choices left a shard with {two} decodes: {:?}",
      held.borrow()
    );
    let one = RefCell::new(vec![0u64; shards.len()]);
    for push in 0..19u32 {
      let key = format!("monorepo push {push}");
      let chosen = intake_shard(arrived, key.as_bytes(), &shards, |shard| {
        if shard == arrived { u64::MAX } else { 0 }
      });
      one.borrow_mut()[usize::from(chosen)] += 1;
    }
    let busiest_one = one.borrow().iter().copied().max().unwrap();
    assert!(
      busiest_one >= two,
      "one choice ({busiest_one}) beat two ({two})"
    );
    // Everything on the owner, today's placement, for scale: all 19 on one shard.
    assert!(two < 19);
  }

  /// A-724 piece 9 (§4.17.5). Do: place every range of a 2-bit, 6-bit and 10-bit cut of one repository over 23
  /// partitions, and another repository's. Expect: every range on one of the partitions, the same answer every time; the
  /// ranges spread (no partition holding more than three times its even share at 10 bits); another family's ranges
  /// placed independently of the first's; none for no partitions.
  #[test]
  fn a_repositorys_index_ranges_spread_over_the_partitions_by_rendezvous() {
    use super::range_partition;
    let partitions: Vec<u16> = (0..23).collect();
    for bits in [2u32, 6, 10] {
      let mut held = vec![0usize; partitions.len()];
      for range in 0..(1u32 << bits) {
        let at = range_partition(b"monorepo-family", range, &partitions).unwrap();
        assert_eq!(
          range_partition(b"monorepo-family", range, &partitions),
          Some(at)
        );
        held[usize::from(at)] += 1;
      }
      if bits == 10 {
        let even = (1usize << bits) / partitions.len();
        assert!(
          held.iter().all(|count| *count <= 3 * even),
          "uneven: {held:?}"
        );
      }
    }
    let differ = (0..64u32)
      .filter(|range| {
        range_partition(b"monorepo-family", *range, &partitions)
          != range_partition(b"another-family", *range, &partitions)
      })
      .count();
    assert!(
      differ > 32,
      "two families placed alike: {differ} of 64 ranges differ"
    );
    assert_eq!(range_partition(b"monorepo-family", 0, &[]), None);
  }

  /// A-724 piece 9. Do: route objects of a repository whose cut is 0, then 4; and step a cut toward what its index's
  /// bytes need. Expect: at 0 every object on the repository's own partition; at 4 objects whose ids share their first
  /// 4 bits on one partition, the 16 ranges spread over several; the cut stepping one bit at a time toward the derived
  /// cut and standing once there.
  #[test]
  fn a_repositorys_index_rows_route_by_its_cut_and_the_cut_steps_one_bit_at_a_time() {
    use super::{index_cut_step, index_partition};
    let partitions: Vec<u16> = (0..23).collect();
    let mut bytes = [0u8; 16];
    bytes[8] = 0;
    bytes[9] = 5;
    let mut record = slates_db::catalog::RepositoryRecord {
      index_bits: 0,
      id: slates_db::catalog::RepositoryId { bytes },
      name: "monorepo".to_owned(),
      tool: slates_db::catalog::VcsTool::Git,
      format: slates_db::catalog::ObjectFormat::Sha1,
      default_branch: None,
      owner: slates_db::catalog::Principal::Certificate { hash: [1; 32] },
      policy: slates_db::catalog::RepositoryPolicy::default(),
    };
    let own = record.id.partition();
    for first in 0..=255u8 {
      assert_eq!(
        index_partition(&record, &[first, 1, 2, 3], &partitions),
        Some(own)
      );
    }
    record.index_bits = 4;
    let a = index_partition(&record, &[0x30, 0, 0, 0], &partitions);
    let b = index_partition(&record, &[0x3f, 9, 9, 9], &partitions);
    assert_eq!(a, b, "one range, one partition");
    let spread: std::collections::BTreeSet<_> = (0..16u8)
      .map(|range| index_partition(&record, &[range << 4], &partitions))
      .collect();
    assert!(spread.len() > 4, "16 ranges on {} partitions", spread.len());
    let row = 41u64;
    let room = 1u64 << 20;
    assert_eq!(index_cut_step(0, 10 * room, room), Some(1));
    assert_eq!(index_cut_step(3, 10 * room, room), Some(4));
    assert_eq!(index_cut_step(4, 10 * room, room), None);
    assert_eq!(index_cut_step(4, room / 2, room), Some(3));
    assert_eq!(index_cut_step(0, 424_851 * row, 746_962_928), None);
  }

  /// A-724. Do: decode on a shard, then let each intake go. Expect: the shard's count rises with each and falls back
  /// to zero as each guard drops; a tie keeps a push on the shard it arrived on.
  #[test]
  fn a_shards_decoding_count_follows_its_guards_and_a_tie_stays_where_it_arrived() {
    let shard = slates_rt::registry::MAX_SHARDS as u16 - 1;
    let before = decoding_on(shard);
    let first = Decoding::begin(shard);
    let second = Decoding::begin(shard);
    assert_eq!(decoding_on(shard), before + 2);
    drop(first);
    drop(second);
    assert_eq!(decoding_on(shard), before);
    assert_eq!(
      intake_shard(4, b"any push", &[4, 5, 6], |_| 3),
      4,
      "a tie stays where it arrived"
    );
  }
}
