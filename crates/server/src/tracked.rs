//! A map that remembers which of its keys changed since its owner last published it (A-270; §4.8 persistence before
//! reply). A shard's recovery publication carries what it holds for other owners as keyed pieces, and a delta carries
//! only the pieces that changed. So the publication must know which changed, and a change it misses is an
//! acknowledged record a warm restart no longer has.
//!
//! The map marks a key changed on every mutable access to it (`get_mut`, `insert`, `remove`, `entry`, `values_mut`), whether or not the value then changes. That over-marks, and an over-marked piece costs one extra piece in
//! one delta. It can never under-mark: no path reaches a value mutably without marking its key, so no call site has to
//! remember to. A caller-kept dirty set would have to be marked at each of the sites that change a held register (15
//! when this was built) and each that changes a live copy (4), and one forgotten site would lose records silently.
//!
//! The marks are bounded by the keys touched since the last publication, and every committed publication clears them
//! ([`Tracked::clear_changed`]); a refused publication keeps them, so its changes ride the next one.

use std::collections::{BTreeMap, BTreeSet, btree_map};

/// A `BTreeMap` that records every key reached mutably since [`Tracked::clear_changed`].
#[derive(Debug)]
pub(crate) struct Tracked<K: Ord + Copy, V> {
  map: BTreeMap<K, V>,
  changed: BTreeSet<K>,
}

impl<K: Ord + Copy, V> Default for Tracked<K, V> {
  fn default() -> Self {
    Tracked {
      map: BTreeMap::new(),
      changed: BTreeSet::new(),
    }
  }
}

impl<K: Ord + Copy, V> Tracked<K, V> {
  /// The value under `key`.
  pub(crate) fn get(&self, key: &K) -> Option<&V> {
    self.map.get(key)
  }

  /// Whether `key` is held.
  pub(crate) fn contains_key(&self, key: &K) -> bool {
    self.map.contains_key(key)
  }

  /// How many keys are held.
  pub(crate) fn len(&self) -> usize {
    self.map.len()
  }

  /// The keys, in order.
  pub(crate) fn keys(&self) -> btree_map::Keys<'_, K, V> {
    self.map.keys()
  }

  /// The values, in key order.
  pub(crate) fn values(&self) -> btree_map::Values<'_, K, V> {
    self.map.values()
  }

  /// The entries, in key order.
  pub(crate) fn iter(&self) -> btree_map::Iter<'_, K, V> {
    self.map.iter()
  }

  /// The map, read-only: what a reader written against a plain map takes. Nothing through it can change a value, so
  /// nothing through it needs a mark.
  pub(crate) fn as_map(&self) -> &BTreeMap<K, V> {
    &self.map
  }

  /// The value under `key`, mutably; marks `key` changed when it is held.
  pub(crate) fn get_mut(&mut self, key: &K) -> Option<&mut V> {
    let value = self.map.get_mut(key)?;
    self.changed.insert(*key);
    Some(value)
  }

  /// Holds `value` under `key`, returning what it replaced; marks `key` changed.
  pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
    self.changed.insert(key);
    self.map.insert(key, value)
  }

  /// Drops `key`, returning its value; marks `key` changed when it was held.
  pub(crate) fn remove(&mut self, key: &K) -> Option<V> {
    let value = self.map.remove(key)?;
    self.changed.insert(*key);
    Some(value)
  }

  /// The entry under `key`; marks `key` changed (the caller may insert or change it).
  pub(crate) fn entry(&mut self, key: K) -> btree_map::Entry<'_, K, V> {
    self.changed.insert(key);
    self.map.entry(key)
  }

  /// Every value, mutably; marks every key changed.
  pub(crate) fn values_mut(&mut self) -> btree_map::ValuesMut<'_, K, V> {
    self.changed.extend(self.map.keys().copied());
    self.map.values_mut()
  }

  /// Test support: drops every entry, as a restart's state begins; marks every key it held changed.
  #[cfg(test)]
  pub(crate) fn clear(&mut self) {
    self.changed.extend(self.map.keys().copied());
    self.map.clear();
  }

  /// The keys reached mutably since the last [`Tracked::clear_changed`], held or since dropped, in order.
  pub(crate) fn changed(&self) -> &BTreeSet<K> {
    &self.changed
  }

  /// Forgets the marks: what they named is published.
  pub(crate) fn clear_changed(&mut self) {
    self.changed.clear();
  }
}

/// A held register's acceptor reached by a takeover's by-name answer (A-251's `answer_by_name`, which raises fences and
/// installs authority): through [`Tracked::get_mut`], so every acceptor it changes is marked, and the change rides the
/// next delta (A-270).
impl slates_db::takeover::AcceptorStore
  for Tracked<slates_db::register::ObjectId, slates_db::register::Acceptor>
{
  fn acceptor_mut(
    &mut self,
    object: &slates_db::register::ObjectId,
  ) -> Option<&mut slates_db::register::Acceptor> {
    self.get_mut(object)
  }
}

impl<'a, K: Ord + Copy, V> IntoIterator for &'a Tracked<K, V> {
  type Item = (&'a K, &'a V);
  type IntoIter = btree_map::Iter<'a, K, V>;

  fn into_iter(self) -> Self::IntoIter {
    self.map.iter()
  }
}

#[cfg(test)]
mod tests {
  use super::Tracked;

  /// A-270: do reach a map's values every mutable way, then publish. Expect each key reached marked, held or dropped,
  /// a read to mark nothing, and the marks gone once cleared, so a delta built from them carries every change.
  #[test]
  fn every_mutable_access_marks_its_key_and_a_read_marks_none() {
    let mut map: Tracked<u8, u32> = Tracked::default();
    map.insert(1, 10);
    map.insert(2, 20);
    map.insert(3, 30);
    map.clear_changed();
    let _ = map.get(&1);
    let _ = map.iter().count();
    assert!(map.changed().is_empty(), "a read marks nothing");
    if let Some(value) = map.get_mut(&1) {
      *value = value.saturating_add(1);
    }
    map.remove(&2);
    let entered = map.entry(4).or_insert(0);
    *entered = entered.saturating_add(1);
    assert_eq!(
      map.changed().iter().copied().collect::<Vec<_>>(),
      vec![1, 2, 4],
      "changed, dropped and entered keys are marked"
    );
    let _ = map.get_mut(&9);
    let _ = map.remove(&9);
    assert!(
      !map.changed().contains(&9),
      "an absent key reached marks nothing"
    );
    map.clear_changed();
    map
      .values_mut()
      .for_each(|value| *value = value.saturating_add(1));
    assert_eq!(map.changed().len(), map.len(), "values_mut marks every key");
  }
}
