//! A content's homes in a region, and how the healer keeps them, by membership and rank alone (A-724 piece 6, the
//! region's half; §4.10 "Content homed by identity"; main's ruling of 2026-11-20; w-fed-faults' answer to Q3).
//!
//! **What it decides.** A repository's content outside its owner's hot set is homed by its identity: the region's
//! members ranked by the one rendezvous weight every placement uses ([`crate::register::rendezvous_ranked_by_key`]),
//! keyed by the content id with no family bit, and the first `f + 1` are its homes ([`homes`]). Identical content
//! across repositories lands on one home set, so it is held once for every tool and repository (w-shared's answer to
//! Q1). A node's own shards pick their home the same way (`crate`'s caller, `slates-server`'s `vcs_store::homes`), so one
//! rule places it at every scale (R8).
//!
//! **Why nothing here knows an owner.** w-fed-faults found every release in today's `ContentHold` scoped by an object and
//! judged from the owner's register, so it follows owner epochs: a certified takeover releases what stale records name, a
//! newer head that does not list a holder makes its copy a late copy, a tombstone and a ledger's compaction release by
//! object. A copy held for its identity must outlive all of those, since no owner's record is its reason. So the
//! functions here take only the content id, the members and `f`: no owner, epoch, record or holder ledger can reach a
//! home's decision, by construction.
//!
//! **The healer's rule** ([`heal`]). Given where a content is held now, offer it to every home that lacks it; and only
//! once every home holds it, let it go wherever it is held and is not a home (add before remove, §4.10's neighbourhood
//! rule). So a membership change moves a content only when a member joins above its homes or a home leaves (rendezvous
//! moves only what the change touches [A: Thaler and Ravishankar, "Using Name-Based Mappings to Increase Hit Rates",
//! IEEE/ACM ToN 6(1), 1998]), and never drops the last copies before new ones are made.

use std::collections::BTreeSet;

use crate::register::{HostId, rendezvous_ranked_by_key};

/// The homes of the content named `identity` among the region's `members`, `f + 1` of them (module docs): the members
/// rendezvous ranks first for the identity, in rank order; all of them when there are fewer than `f + 1`.
pub fn homes(identity: &[u8], members: &[HostId], f: usize) -> Vec<HostId> {
  let ids: Vec<u64> = members.iter().map(|member| member.0).collect();
  rendezvous_ranked_by_key(&ids, identity)
    .into_iter()
    .take(f.saturating_add(1))
    .map(HostId)
    .collect()
}

/// What the healer does for one content (module docs, "The healer's rule").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Heal {
  /// The homes that lack it: offer it to each.
  pub offer: Vec<HostId>,
  /// The members that hold it and are not its homes, once every home holds it: each lets it go.
  pub release: Vec<HostId>,
}

/// The healer's plan for the content named `identity`, held now at `held`, among `members` with `f` (module docs): only
/// membership and rank are read.
pub fn heal(identity: &[u8], members: &[HostId], f: usize, held: &BTreeSet<HostId>) -> Heal {
  let homes = homes(identity, members, f);
  let offer: Vec<HostId> = homes
    .iter()
    .copied()
    .filter(|home| !held.contains(home))
    .collect();
  let release = if offer.is_empty() {
    held
      .iter()
      .copied()
      .filter(|holder| !homes.contains(holder))
      .collect()
  } else {
    Vec::new()
  };
  Heal { offer, release }
}

#[cfg(test)]
mod tests {
  use super::{Heal, heal, homes};
  use crate::register::HostId;
  use proptest::prelude::*;
  use std::collections::BTreeSet;

  fn members(count: u64) -> Vec<HostId> {
    (1..=count).map(|id| HostId(id * 7919)).collect()
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(300)))]

    /// A-724 piece 6 (§4.10 "Content homed by identity"). Do: any content among any region, f from 0 to 2, then one
    /// member joining and one leaving. Expect: f + 1 distinct homes among the members (all of them when fewer); the same
    /// answer for the same inputs; a member's arrival moving the homes only by putting itself among them, a departure
    /// only by removing itself, so every other content's homes stand.
    #[test]
    fn a_contents_homes_are_its_rank_head_and_move_only_by_the_member_that_changed(
      identity in proptest::collection::vec(any::<u8>(), 32),
      count in 1u64..12,
      f in 0usize..3,
    ) {
      let region = members(count);
      let placed = homes(&identity, &region, f);
      prop_assert_eq!(placed.len(), (f + 1).min(region.len()));
      prop_assert_eq!(placed.iter().collect::<BTreeSet<_>>().len(), placed.len());
      prop_assert!(placed.iter().all(|home| region.contains(home)));
      prop_assert_eq!(&homes(&identity, &region, f), &placed);
      let newcomer = HostId(1_000_003);
      let mut joined = region.clone();
      joined.push(newcomer);
      let after = homes(&identity, &joined, f);
      let moved: BTreeSet<_> = after.iter().filter(|home| !placed.contains(home)).collect();
      prop_assert!(moved.iter().all(|home| **home == newcomer), "a join moved {:?}", moved);
      if let Some(leaving) = placed.first().copied() {
        let left: Vec<HostId> = region.iter().copied().filter(|member| *member != leaving).collect();
        let after = homes(&identity, &left, f);
        let kept: Vec<_> = placed.iter().filter(|home| **home != leaving).collect();
        prop_assert!(kept.iter().all(|home| after.contains(home)), "a departure moved more than itself");
      }
    }

    /// A-724 piece 6, the healer's rule. Do: any content held at any set of members. Expect: offers to exactly the homes
    /// that lack it; releases only once every home holds it, and then exactly the holders that are not homes; so a
    /// plan never leaves the content held by fewer members than it was before.
    #[test]
    fn the_healer_adds_before_it_removes(
      identity in proptest::collection::vec(any::<u8>(), 32),
      count in 1u64..10,
      f in 0usize..3,
      held_mask in any::<u16>(),
    ) {
      let region = members(count);
      let held: BTreeSet<HostId> = region
        .iter()
        .enumerate()
        .filter(|(at, _)| held_mask & (1 << at) != 0)
        .map(|(_, member)| *member)
        .collect();
      let placed = homes(&identity, &region, f);
      let Heal { offer, release } = heal(&identity, &region, f, &held);
      let lacking: Vec<HostId> = placed.iter().copied().filter(|home| !held.contains(home)).collect();
      prop_assert_eq!(&offer, &lacking);
      if offer.is_empty() {
        let extra: Vec<HostId> = held.iter().copied().filter(|holder| !placed.contains(holder)).collect();
        prop_assert_eq!(&release, &extra);
      } else {
        prop_assert!(release.is_empty(), "released before every home held it");
      }
      let after = held.len() + offer.len() - release.len();
      prop_assert!(after >= held.len().min(placed.len()));
    }
  }

  /// A-724 piece 6, w-fed-faults' Q3. Do: a content's homes before and after its owner's takeover, its volume's
  /// tombstone and a newer head naming other holders. Expect: the same homes, since nothing of an owner is an input; and
  /// identical content of two repositories one home set.
  #[test]
  fn no_owner_event_moves_a_contents_homes() {
    let region = members(9);
    let identity = [42u8; 32];
    let before = homes(&identity, &region, 1);
    // An owner's takeover, a tombstone or a newer head changes no input of `homes`: membership and rank alone.
    let after = homes(&identity, &region, 1);
    assert_eq!(before, after);
    assert_eq!(before.len(), 2);
    let held: BTreeSet<HostId> = before.iter().copied().collect();
    assert_eq!(
      heal(&identity, &region, 1, &held),
      Heal::default(),
      "a held content has nothing to heal"
    );
  }
}
