//! The per-object routing view (§4.8 "Promotion and takeover", D-14): which objects this node holds a
//! copy of, and the owner each has now. It complements the regional configuration
//! ([`crate::config_group::RegionalCouncil`]), whose per-owner `Configuration` models each node's authority
//! over its *own* objects; a holder instead needs to know who owns the *peers'* objects it backs.
//!
//! Placement is computed by rendezvous, never stored in a directory (D-14: "ids route to owners"; no global
//! catalog, D-12), so this holds only an `object → current owner` map for the objects this node actually holds
//! — its own adopted ones and the peers' it backs — never every object in the region. Who takes a retired
//! owner's object over is **not** decided here: the regional configuration names it from the retired owner's
//! settled neighbourhood and the members it left
//! ([`slates_db::register::RegionalConfiguration::lineage`]), so every node names the same successor
//! whatever it holds or remembers, and the node that installs a retirement moves the object here with
//! [`Routing::track`]. An earlier form kept each record's cohort as the holder had last seen it and ranked the
//! successor from that, and holders that last saw different cohorts named different successors
//! (`docs/bugs/2026-09-29-a-takeover-stalled-when-a-survivor-never-received-the-head.md`).

use std::collections::BTreeMap;

use slates_db::register::{HostId, ObjectId};

/// The objects this node holds a copy of, keyed to their current owner. Bounded by what this node
/// actually holds (its own adopted objects and the peers' it backs), not the region's whole object set.
#[derive(Debug)]
pub struct Routing {
  owners: BTreeMap<ObjectId, HostId>,
}

impl Routing {
  /// A routing view holding nothing yet.
  pub fn new() -> Routing {
    Routing {
      owners: BTreeMap::new(),
    }
  }

  /// Records that `owner` owns `object` now: an accepted record's owner, an adoption, or the successor a
  /// retirement names.
  pub fn track(&mut self, object: ObjectId, owner: HostId) {
    self.owners.insert(object, owner);
  }

  /// Forgets `object` (destroyed, reclaimed, or no longer held here).
  pub fn forget(&mut self, object: ObjectId) {
    self.owners.remove(&object);
  }

  /// Forgets every object `owner` owns — this node's own entries once a retirement took them over.
  pub fn forget_owned_by(&mut self, owner: HostId) {
    self.owners.retain(|_, current| *current != owner);
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
}

impl Default for Routing {
  fn default() -> Routing {
    Routing::new()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const SELF: HostId = HostId(1);
  const PEER: HostId = HostId(2);

  /// Tracking records an object's owner, tracking again moves it, and forgetting removes it.
  #[test]
  fn tracking_records_and_moves_the_owner_and_forgetting_removes_it() {
    let mut routing = Routing::new();
    assert!(routing.is_empty());
    let object = ObjectId::new(PEER, 0);
    routing.track(object, PEER);
    assert_eq!(routing.owner_of(object), Some(PEER));
    routing.track(object, SELF);
    assert_eq!(routing.owner_of(object), Some(SELF), "a takeover moves it");
    assert_eq!(routing.len(), 1);
    routing.forget(object);
    assert_eq!(routing.owner_of(object), None);
    assert!(routing.is_empty());
  }

  /// Forgetting an owner's objects removes exactly those, leaving every other owner's.
  #[test]
  fn forgetting_an_owners_objects_leaves_the_rest() {
    let mut routing = Routing::new();
    let own = ObjectId::new(SELF, 1);
    let backed = ObjectId::new(PEER, 1);
    routing.track(own, SELF);
    routing.track(backed, PEER);
    routing.forget_owned_by(SELF);
    assert_eq!(routing.owner_of(own), None);
    assert_eq!(routing.owner_of(backed), Some(PEER));
  }
}
