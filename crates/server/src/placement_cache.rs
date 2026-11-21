//! Each shard's placements of the objects it asks about, one computation a configuration (§4.8 "Placement"; 85a, g228).
//!
//! **Why.** `Configuration::place` derives an object's candidates from the owner, its neighbourhood, the failure
//! domains, the quorum and the settled cohort: deterministic, but each call builds its copysets anew, about a dozen
//! allocations. A live write asked it on every durability poll and every resolve of every write (`live_ship`'s
//! `head_placed`, `catalog_placed_at`, `remote_candidates`, `follow_candidates`, `catalogued_holders`): 250 of the 284
//! allocator calls a 2 KiB live write cost its owner, against 34 for a plain write (counted per thread,
//! `a_live_writes_allocations_on_its_owner_are_counted_against_a_plain_ones` in `crates/server/tests/fleet.rs`).
//!
//! **What it holds.** Each placement computed under the configuration in force, kept until anything `place` reads
//! changes: the key is all of it (version, generation, owner, neighbourhood, domains, quorum, settled cohort),
//! compared whole on every look, so a configuration that changed without a new version still misses. Bounded by the
//! shard's placed heads (`ShardState::placed_heads`): a cache that has outgrown them is cleared and refilled.

use std::collections::BTreeMap;

use slates_db::register::{Configuration, ObjectId, Placement};

/// Everything [`Configuration::place`] reads, as the cache was filled under.
#[derive(Debug, PartialEq, Eq)]
struct Key {
  version: u64,
  generation: u64,
  owner: slates_db::register::HostId,
  neighbourhood: Vec<slates_db::register::HostId>,
  domains: BTreeMap<slates_db::register::HostId, slates_db::register::DomainId>,
  quorum: slates_db::register::Quorum,
  settled: Option<slates_db::register::Settled>,
}

impl Key {
  /// Whether `configuration` places objects as this key's did: compared field by field, nothing allocated.
  fn matches(&self, configuration: &Configuration) -> bool {
    self.version == configuration.version
      && self.generation == configuration.generation
      && self.owner == configuration.owner
      && self.quorum == configuration.quorum
      && self.neighbourhood == configuration.neighbourhood
      && self.domains == configuration.domains
      && self.settled == configuration.settled
  }

  /// The key of `configuration`.
  fn of(configuration: &Configuration) -> Key {
    Key {
      version: configuration.version,
      generation: configuration.generation,
      owner: configuration.owner,
      neighbourhood: configuration.neighbourhood.clone(),
      domains: configuration.domains.clone(),
      quorum: configuration.quorum,
      settled: configuration.settled.clone(),
    }
  }
}

/// The cache.
#[derive(Debug, Default)]
pub(crate) struct PlacementCache {
  key: Option<Key>,
  placements: BTreeMap<ObjectId, Placement>,
}

impl PlacementCache {
  /// Runs `with` on `object`'s placement under `configuration`, computed once a configuration; `bound` is how many
  /// objects the cache may hold before it is cleared (the shard's placed heads).
  pub(crate) fn with<R>(
    &mut self,
    configuration: &Configuration,
    object: ObjectId,
    bound: usize,
    with: impl FnOnce(&Placement) -> R,
  ) -> R {
    if self
      .key
      .as_ref()
      .is_none_or(|key| !key.matches(configuration))
    {
      self.key = Some(Key::of(configuration));
      self.placements.clear();
    }
    let bound = bound.max(1);
    let full = self.placements.len() >= bound && !self.placements.contains_key(&object);
    if full || self.placements.len() > bound {
      self.placements.clear();
    }
    let placement = self
      .placements
      .entry(object)
      .or_insert_with(|| configuration.place(object));
    with(placement)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_db::register::{HostId, Quorum};

  /// A three-host neighbourhood at `f = 1` owned by host 1, at `version`.
  fn fleet(version: u64, neighbourhood: &[u64]) -> Configuration {
    let mut configuration = Configuration::solo(HostId(1));
    configuration.version = version;
    configuration.quorum = Quorum { f: 1 };
    configuration.neighbourhood = neighbourhood.iter().copied().map(HostId).collect();
    configuration
  }

  /// 85a (g228). Do: ask the cache for several objects' placements under one configuration, then under one whose
  /// neighbourhood changed at the same version, then at a new version; and ask past its bound. Expect every answer equal
  /// to `Configuration::place` under the configuration asked with: the cache never answers for a configuration it was
  /// not filled under, however the change came, and a full cache still answers right.
  #[test]
  fn a_cached_placement_is_always_the_configurations_own() {
    let objects: Vec<ObjectId> = (0u8..8).map(|byte| ObjectId([byte; 16])).collect();
    let mut cache = PlacementCache::default();
    for configuration in [
      fleet(5, &[1, 2, 3]),
      fleet(5, &[1, 2, 4]),
      fleet(6, &[1, 2, 4, 5, 6]),
    ] {
      for object in &objects {
        let cached = cache.with(&configuration, *object, objects.len(), Clone::clone);
        assert_eq!(
          cached,
          configuration.place(*object),
          "{object:?} under {configuration:?}"
        );
      }
      // Past its bound: cleared and refilled, still the configuration's own.
      let bound = 2;
      for object in &objects {
        let cached = cache.with(&configuration, *object, bound, Clone::clone);
        assert_eq!(cached, configuration.place(*object));
        assert!(
          cache.placements.len() <= bound,
          "the cache stays within its bound"
        );
      }
    }
  }
}
