//! The per-object routing view (§4.8 "Promotion and takeover", D-14): which objects this node holds a
//! copy of, and — when a peer leaves the neighbourhood — which of that peer's objects this node takes
//! over. It complements [`crate::config_group::ConfigGroup`], whose single-owner `Configuration` models
//! *this* node's authority over its *own* objects; a cross-node takeover instead asks about a *peer's*
//! objects, which needs a per-object view of who owns what this node participates in.
//!
//! Placement is computed by rendezvous, never stored in a directory (D-14: "ids route to owners"; no
//! global catalog, D-12), so this holds only an `object → current owner` map for the objects this node
//! actually holds — its own, and the peers' it backs — never every object in the region. The takeover
//! winner is recomputed the same way every host computes placement:
//! [`slates_db::register::rendezvous_first`] over the object's *surviving holders* — the copyset
//! [`slates_db::register::candidates_for`] places it on, minus the dead host — so the survivor a dead
//! owner's object falls to is agreed with no coordination (the worked example's "rendezvous ranks first
//! among {B, C, D}"), and is always a host that held a copy. A single-generation takeover's fencing and
//! the phase-one recovery of the dead owner's
//! head live in [`crate::config_group`] and the register; this module answers only *which* objects.

use std::collections::BTreeMap;

use slates_db::register::{DomainId, HostId, ObjectId, Quorum, candidates_for, rendezvous_first};

/// The objects this node holds a copy of, keyed to their current owner. Bounded by what this node
/// actually holds (its own objects and the peers' it backs), not the region's whole object set.
#[derive(Debug)]
pub struct Routing {
  self_host: HostId,
  owners: BTreeMap<ObjectId, HostId>,
}

/// One object a takeover moved: its id and the owner it now has (this node, for a `taken` object).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reassignment {
  /// The object.
  pub object: ObjectId,
  /// The object's new owner after the dead host left.
  pub new_owner: HostId,
}

impl Routing {
  /// A routing view for `self_host` holding nothing yet.
  pub fn new(self_host: HostId) -> Routing {
    Routing {
      self_host,
      owners: BTreeMap::new(),
    }
  }

  /// Records that this node holds `object`, currently owned by `owner` (this node's own object, or a
  /// peer's this node backs as a candidate). Idempotent; a later call updates the recorded owner.
  pub fn track(&mut self, object: ObjectId, owner: HostId) {
    self.owners.insert(object, owner);
  }

  /// Forgets `object` (destroyed, or no longer held here).
  pub fn forget(&mut self, object: ObjectId) {
    self.owners.remove(&object);
  }

  /// The current owner of `object` as this node sees it, or `None` if this node does not hold it.
  pub fn owner_of(&self, object: ObjectId) -> Option<HostId> {
    self.owners.get(&object).copied()
  }

  /// How many objects this node holds a copy of.
  pub fn len(&self) -> usize {
    self.owners.len()
  }

  /// Whether this node holds any object.
  pub fn is_empty(&self) -> bool {
    self.owners.is_empty()
  }

  /// Folds a `dead` host leaving the `neighbourhood` (which still lists `dead`) into the routing view:
  /// for each object this node holds that `dead` owned, the new owner is the survivor that rendezvous
  /// ranks first **among the object's holders** — the copyset `candidates_for` placed it on (under `dead`
  /// as owner, at `quorum`), minus `dead`. Every survivor runs the same computation, so all agree on who
  /// takes each object, and the winner always held a copy: above the candidate floor a neighbourhood host
  /// outside the object's copyset never held it and must never be named its owner. The recorded owner is
  /// updated for every such object; the ones that fall to *this* node are returned as the takeovers this
  /// node must drive (phase-one recovery then serving). An object whose every holder left with `dead` is
  /// dropped (its last copy is gone).
  pub fn take_over(
    &mut self,
    dead: HostId,
    neighbourhood: &[HostId],
    domains: &BTreeMap<HostId, DomainId>,
    quorum: Quorum,
  ) -> Vec<Reassignment> {
    let affected: Vec<ObjectId> = self
      .owners
      .iter()
      .filter(|(_, owner)| **owner == dead)
      .map(|(object, _)| *object)
      .collect();
    let mut mine = Vec::new();
    for object in affected {
      // The object's surviving holders: its copyset under the dead owner, minus the dead host.
      let survivors: Vec<HostId> = candidates_for(dead, neighbourhood, domains, object, quorum)
        .into_iter()
        .filter(|host| *host != dead)
        .collect();
      match rendezvous_first(&survivors, object) {
        Some(new_owner) => {
          self.owners.insert(object, new_owner);
          if new_owner == self.self_host {
            mine.push(Reassignment { object, new_owner });
          }
        }
        // No surviving holder — the object's every copy left with the dead host; this node cannot serve it.
        None => {
          self.owners.remove(&object);
        }
      }
    }
    mine
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeSet;

  use super::*;

  const SELF: HostId = HostId(1);
  const PEER: HostId = HostId(2);
  const OTHER: HostId = HostId(3);

  /// Tracking records an object's owner; forgetting removes it.
  #[test]
  fn tracking_records_the_owner_and_forgetting_removes_it() {
    let mut routing = Routing::new(SELF);
    assert!(routing.is_empty());
    let object = ObjectId::new(SELF, 0);
    routing.track(object, SELF);
    assert_eq!(routing.owner_of(object), Some(SELF));
    assert_eq!(routing.len(), 1);
    routing.forget(object);
    assert_eq!(routing.owner_of(object), None);
    assert!(routing.is_empty());
  }

  /// A dead host's death moves only the objects it owned: the survivors that rendezvous-rank first take
  /// them (this node the ones that fall to it), the recorded owner is updated for every moved object, and
  /// objects this node owns are untouched. Non-vacuous: across many objects the dead peer owned, some
  /// fall to this node and some to another survivor (rendezvous spreads them), so takeover is a real
  /// split, not all-or-nothing.
  #[test]
  fn a_death_reassigns_the_dead_owners_objects_by_rendezvous() {
    let neighbourhood = vec![SELF, PEER, OTHER];
    let domains = std::collections::BTreeMap::new(); // unique-per-host
    let mut routing = Routing::new(SELF);

    // This node holds one of its own objects and backs many of the peer's.
    let own = ObjectId::new(SELF, 7);
    routing.track(own, SELF);
    let peer_objects: Vec<ObjectId> = (0..64u64).map(|i| ObjectId::new(PEER, i)).collect();
    for &object in &peer_objects {
      routing.track(object, PEER);
    }

    let taken: BTreeSet<ObjectId> = routing
      .take_over(PEER, &neighbourhood, &domains, Quorum { f: 1 })
      .into_iter()
      .map(|r| r.object)
      .collect();

    // This node's own object is untouched.
    assert_eq!(
      routing.owner_of(own),
      Some(SELF),
      "our own object is not moved"
    );
    // Each peer object was reassigned to the survivor that rendezvous-ranks first — the same
    // computation every host runs — and is returned as taken exactly when that survivor is this node.
    let survivors = [SELF, OTHER];
    for &object in &peer_objects {
      let owner = routing.owner_of(object).expect("still tracked");
      assert_eq!(
        rendezvous_first(&survivors, object),
        Some(owner),
        "reassigned to the rendezvous-first survivor (never the dead {PEER:?})"
      );
      assert_eq!(
        taken.contains(&object),
        owner == SELF,
        "taken iff reassigned to self"
      );
    }
    // Non-vacuity: the split is real — this node took some and the other survivor took the rest.
    assert!(
      !taken.is_empty() && taken.len() < peer_objects.len(),
      "rendezvous spread the objects across both survivors, not all-or-nothing"
    );
  }

  /// When no survivor remains for an object (its last holder was the dead host), it is dropped — this
  /// node cannot serve an object it holds no copy of.
  #[test]
  fn an_object_with_no_survivor_is_dropped() {
    // Neighbourhood is just the dead peer (a degenerate the guard must handle, not panic).
    let mut routing = Routing::new(SELF);
    let orphan = ObjectId::new(PEER, 0);
    routing.track(orphan, PEER);
    let taken = routing.take_over(
      PEER,
      &[PEER],
      &std::collections::BTreeMap::new(),
      Quorum { f: 1 },
    );
    assert!(taken.is_empty());
    assert_eq!(
      routing.owner_of(orphan),
      None,
      "no survivor: the object is dropped"
    );
  }

  /// AC (§4.8 "Promotion and takeover", D-14): above the candidate floor a dead owner's objects are
  /// reassigned only to a host in each object's own copyset — one that actually held a copy — never to a
  /// neighbourhood host outside it. This is the copyset-consistent takeover the NoLoss invariant needs: a
  /// wide neighbourhood splits into several copysets, and each object's successor is drawn from the right
  /// one, not by a single rendezvous over the whole neighbourhood (which could name a non-holder).
  #[test]
  fn a_takeover_above_the_floor_stays_within_each_objects_copyset() {
    let dead = HostId(10);
    // The dead owner plus four co-holders at f=1 → 2f=2 per copyset → two fixed copysets (above the floor
    // of three), so a host in one copyset never held an object placed on the other.
    let neighbourhood = vec![dead, SELF, OTHER, HostId(4), HostId(5)];
    let domains = std::collections::BTreeMap::new(); // unique-per-host
    let quorum = Quorum { f: 1 };
    let mut routing = Routing::new(SELF);
    let objects: Vec<ObjectId> = (0..128u64).map(|i| ObjectId::new(dead, i)).collect();
    for &object in &objects {
      routing.track(object, dead);
    }

    routing.take_over(dead, &neighbourhood, &domains, quorum);

    let mut owners = BTreeSet::new();
    let mut copysets = BTreeSet::new();
    for &object in &objects {
      let new_owner = routing.owner_of(object).expect("reassigned, not dropped");
      let copyset = candidates_for(dead, &neighbourhood, &domains, object, quorum);
      assert!(
        copyset.contains(&new_owner),
        "the successor {new_owner:?} must have held the object (be in its copyset {copyset:?})"
      );
      assert_ne!(new_owner, dead, "never the dead owner");
      owners.insert(new_owner);
      copysets.insert(copyset);
    }
    // Non-vacuity: the objects really split across more than one copyset (so the within-copyset check is not
    // trivially met by a single global set), and their successors span more than one host.
    assert!(
      copysets.len() > 1,
      "the wide neighbourhood split into multiple copysets"
    );
    assert!(
      owners.len() > 1,
      "successors spread across copysets, not funnelled to one host"
    );
  }
}
