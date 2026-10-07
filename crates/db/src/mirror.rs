//! Asynchronous mirroring to a second region (§4.8 "Mirroring"; D-18). A volume's committed
//! records and their content reach a mirror region asynchronously, in epoch order, with a measured,
//! exposed lag; an operation that needs them there awaits the mirror scope (`await placed(mirror)`,
//! [`crate::register::DurabilityScope::Mirror`]). This module is that shipper, built entirely on the
//! fenced ledger register of [`crate::ledger`]: the mirror region is a second [`Cohort`] with its
//! own writer, and mirroring replays the home region's committed prefix onto it in position order —
//! which is epoch order, because the committed prefix commits position by position and its epochs
//! never decrease.
//!
//! It is pure and deterministic (R8): the mirror's holders are in memory and shipping is a direct
//! call, so the same code runs on one machine (the mirror region's own `f`) and across regions,
//! with no network and no clock. The lag is exposed as a count of records committed at home but not
//! yet at the mirror; `await placed(mirror)` for a record is true once the mirror has committed
//! through its position. A laptop has no mirror region ([`crate::register::Configuration`] refuses
//! the scope), so this type is instantiated only where one exists.
//!
//! The mirror is downstream, never a second source of truth: it only ever adopts the home region's
//! committed records, in order, so it can lag but never diverge — a partition of the mirror's own
//! holders stalls its lag without ever committing a record the home did not.

use std::collections::BTreeMap;

use crate::ledger::{Cohort, Owner, Reach};
use crate::register::{
  DomainId, HostId, ObjectId, Quorum, RegionId, RootConfiguration, rendezvous_ranked,
};

/// A volume's mirror region: a cohort of the mirror's candidate holders and the writer that replays
/// the home region's committed records onto them in epoch order.
#[derive(Clone, Debug)]
pub struct Mirror {
  cohort: Cohort,
  writer: Owner,
}

impl Mirror {
  /// The mirror region for `object`: its own `2f + 1` candidate holders drawn from the mirror
  /// region's neighbourhood, with an empty log. The `quorum` is the mirror region's fault-domain
  /// tree, independent of the home region's.
  pub fn new(
    owner: HostId,
    neighbourhood: &[HostId],
    domains: &std::collections::BTreeMap<HostId, DomainId>,
    object: ObjectId,
    quorum: Quorum,
  ) -> Mirror {
    let cohort = Cohort::new(owner, neighbourhood, domains, object, quorum);
    let writer = Owner::bootstrap(&cohort);
    Mirror { cohort, writer }
  }

  /// The mirror region's candidate holders, owner first.
  pub fn candidates(&self) -> &[HostId] {
    self.cohort.candidates()
  }

  /// The number of records committed on the mirror.
  pub fn committed_len(&self) -> usize {
    self.cohort.committed_prefix().len()
  }

  /// The mirror lag against a home region whose committed prefix is `home_committed`: the records
  /// committed at home but not yet at the mirror. Zero when the mirror has caught up. The exposed
  /// lag of D-18 (a count here; a time or an epoch distance once the runtime carries clocks).
  pub fn lag(&self, home_committed: &[[u8; 32]]) -> usize {
    home_committed.len().saturating_sub(self.committed_len())
  }

  /// Whether the mirror has committed through `position` — `await placed(mirror)` for the record at
  /// that position. False while the record has not yet been shipped and committed on the mirror.
  pub fn placed_through(&self, position: usize) -> bool {
    self.committed_len() > position
  }

  /// Ships the home region's committed records that the mirror lacks, in order, to the reachable
  /// mirror holders, committing each at the mirror's own `f + 1`. Returns the number of records
  /// newly committed on the mirror. Replaying the home's committed prefix is epoch order, so the
  /// mirror commits a dense prefix and never a record before an earlier one; when the mirror's own
  /// holders are partitioned below their quorum nothing new commits (the lag holds), and a later
  /// ship after the partition heals catches the mirror up — the replay is idempotent and resumable.
  pub fn ship(&mut self, home_committed: &[[u8; 32]], reachable: &Reach) -> usize {
    let before = self.committed_len();
    let after = self
      .writer
      .replicate(&mut self.cohort, home_committed, reachable);
    after.saturating_sub(before)
  }
}

/// The mirror cohort of `object` (`docs/wip/mirroring.md` decision 1): the first `2f + 1` of `members` — the mirror
/// region's members the owner holds alive — in rendezvous order, each once. The owner puts to the first `f + 1` and
/// hedges to the rest; the hosts that acknowledge are named in the mirrored record, so nothing later depends on
/// another node computing the same cohort. Fewer members than `2f + 1` give them all.
pub fn mirror_cohort(object: ObjectId, members: &[HostId], quorum: Quorum) -> Vec<HostId> {
  let mut unique = members.to_vec();
  unique.sort_unstable();
  unique.dedup();
  let mut ranked = rendezvous_ranked(&unique, object);
  ranked.truncate(quorum.candidates());
  ranked
}

/// Why a mirror holder refuses a put of a record or content (`docs/wip/mirroring.md` decision 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MirrorRefusal {
  /// The object's home region declares no mirror, or another region than the holder's.
  NotItsMirror {
    /// The object's home region.
    home: RegionId,
  },
  /// The object's home region was promoted: the mirror now serves it, and the lost region writes nothing to it.
  HomePromoted {
    /// The promoted (lost) region.
    home: RegionId,
  },
  /// The sender is not a member of the object's home region, or its region is not declared.
  SenderOutsideHome {
    /// The object's home region.
    home: RegionId,
  },
}

/// Whether a holder in `own_region` admits a mirror put of `object` from `sender` (`docs/wip/mirroring.md` decision
/// 2): the object's home region — its creator's region, moved by the root configuration's homes — is not promoted,
/// declares `own_region` its mirror, and holds the sender. `regions` is every member's declared region (the
/// manifest's); a member with none declared is region 0, as everywhere else the fleet reads it.
pub fn admits_mirror_put(
  root: &RootConfiguration,
  mirrors: &BTreeMap<RegionId, RegionId>,
  regions: &BTreeMap<HostId, RegionId>,
  own_region: RegionId,
  sender: HostId,
  object: ObjectId,
) -> Result<(), MirrorRefusal> {
  let region_of = |host: HostId| regions.get(&host).copied().unwrap_or(RegionId(0));
  let home = root
    .homes
    .get(&object)
    .copied()
    .unwrap_or_else(|| region_of(object.creator()));
  if root.promotions.contains_key(&home) {
    return Err(MirrorRefusal::HomePromoted { home });
  }
  if mirrors.get(&home) != Some(&own_region) || home == own_region {
    return Err(MirrorRefusal::NotItsMirror { home });
  }
  if region_of(sender) != home {
    return Err(MirrorRefusal::SenderOutsideHome { home });
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  const F1: Quorum = Quorum { f: 1 };

  fn root(regions: &[u64]) -> RootConfiguration {
    RootConfiguration::formed(regions.iter().map(|r| RegionId(*r)).collect())
  }

  /// Hosts 1-3 in region 0 and 11-13 in region 1, each region the other's mirror.
  fn two_regions() -> (BTreeMap<RegionId, RegionId>, BTreeMap<HostId, RegionId>) {
    let mirrors = [(RegionId(0), RegionId(1)), (RegionId(1), RegionId(0))]
      .into_iter()
      .collect();
    let regions = [1, 2, 3, 11, 12, 13]
      .into_iter()
      .map(|h| (HostId(h), RegionId(if h > 10 { 1 } else { 0 })))
      .collect();
    (mirrors, regions)
  }

  /// docs/wip/mirroring.md decision 1. Do: take the cohort of many objects over five members, with duplicates and
  /// shuffled order. Expect: `2f + 1` distinct members, exactly the rendezvous order's prefix, whatever the input
  /// order; fewer members give them all.
  #[test]
  fn the_cohort_is_the_rendezvous_prefix_of_the_members_whatever_their_order() {
    let members: Vec<HostId> = [11, 12, 13, 14, 15].into_iter().map(HostId).collect();
    let mut shuffled = members.clone();
    shuffled.reverse();
    shuffled.push(HostId(12));
    for local in 0..64 {
      let object = ObjectId::new(HostId(1), local);
      let cohort = mirror_cohort(object, &shuffled, F1);
      assert_eq!(cohort.len(), 3);
      assert_eq!(cohort, rendezvous_ranked(&members, object)[..3].to_vec());
      assert_eq!(mirror_cohort(object, &members[..2], F1).len(), 2);
    }
  }

  /// docs/wip/mirroring.md decision 2, every clause against a table. Do: ask for a region-0 volume's mirror put at
  /// holders of each region from senders of each region, before and after region 0's promotion, and for a volume
  /// whose home moved. Expect: admitted only at the home's mirror, from a home member, while the home stands.
  #[test]
  fn a_mirror_put_is_admitted_only_at_the_homes_mirror_from_a_home_member_while_the_home_stands() {
    let (mirrors, regions) = two_regions();
    let object = ObjectId::new(HostId(2), 7);
    let mut root = root(&[0, 1]);
    assert_eq!(
      admits_mirror_put(&root, &mirrors, &regions, RegionId(1), HostId(3), object),
      Ok(())
    );
    assert_eq!(
      admits_mirror_put(&root, &mirrors, &regions, RegionId(0), HostId(3), object),
      Err(MirrorRefusal::NotItsMirror { home: RegionId(0) }),
      "a holder in the home region is no mirror"
    );
    assert_eq!(
      admits_mirror_put(&root, &mirrors, &regions, RegionId(1), HostId(12), object),
      Err(MirrorRefusal::SenderOutsideHome { home: RegionId(0) }),
      "a sender outside the home writes nothing"
    );
    assert_eq!(
      admits_mirror_put(
        &root,
        &BTreeMap::new(),
        &regions,
        RegionId(1),
        HostId(3),
        object
      ),
      Err(MirrorRefusal::NotItsMirror { home: RegionId(0) }),
      "no declared mirror"
    );
    // A volume whose home moved to region 1 mirrors to region 0, from region-1 senders.
    root.homes.insert(object, RegionId(1));
    assert_eq!(
      admits_mirror_put(&root, &mirrors, &regions, RegionId(0), HostId(12), object),
      Ok(())
    );
    assert_eq!(
      admits_mirror_put(&root, &mirrors, &regions, RegionId(0), HostId(3), object),
      Err(MirrorRefusal::SenderOutsideHome { home: RegionId(1) })
    );
    root.homes.remove(&object);
    root.promotions.insert(RegionId(0), RegionId(1));
    assert_eq!(
      admits_mirror_put(&root, &mirrors, &regions, RegionId(1), HostId(3), object),
      Err(MirrorRefusal::HomePromoted { home: RegionId(0) }),
      "a promoted region writes nothing to its mirror"
    );
  }
}
