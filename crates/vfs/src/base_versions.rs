//! The base plane's witness tables, versioned by epoch (§4.5, §4.15, A-48). A witnessed base, the disk
//! home it was taken at, and the fingerprint behind a whiteout or a redirect are recorded against the
//! head's epoch when they change, so a snapshot — and a clone made from one — reads them as the snapshot
//! froze them, the way it already reads the directory tree and the inode table (D-5). A landing verdict is
//! a pure function of the witnessed base, the disk now and the overlay (D-26), so a snapshot whose overlay
//! is frozen but whose witnesses are the head's is judged against a base it was never based on.
//!
//! Before 2026-09-30 these tables were the head's only. A landing of an older snapshot had no witnesses to
//! judge its entries by (AUD-29-02), and a clone of an older snapshot inherited the head's: after the head
//! rewitnessed an entry (the disk changed and the agent took the change in), a clone of a snapshot from
//! before judged its old-based bytes against the newer witness, found the disk "unchanged", and could land
//! over the outsider's change without a conflict.
//!
//! Shape: per key, its versions in epoch order, each the key's value from that epoch on (`None` once it is
//! removed); a snapshot frozen at epoch `e` reads the last version from at or before `e`. A write
//! overwrites the key's last version when no live snapshot can see it (it is from after the newest live
//! snapshot) and appends one otherwise. Bounded at every step: each version but a key's last is the one
//! some live snapshot reads, so a key holds at most one more version than the live snapshots; a
//! snapshot's destroy drops the versions only it read, and a key left with nothing but removals is
//! dropped.

use std::collections::BTreeMap;

use crate::ids::Epoch;

/// A key's value from one epoch on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Version<V> {
  /// The epoch the value holds from.
  pub(crate) from: Epoch,
  /// The value, or `None` from its removal on.
  pub(crate) value: Option<V>,
}

/// A table whose keys keep their values as of every epoch a snapshot of the volume still names.
#[derive(Clone, Debug)]
pub(crate) struct Versioned<K, V> {
  keys: BTreeMap<K, Vec<Version<V>>>,
}

impl<K, V> Default for Versioned<K, V> {
  fn default() -> Self {
    Self {
      keys: BTreeMap::new(),
    }
  }
}

impl<K: Ord + Clone, V: Clone + PartialEq> Versioned<K, V> {
  /// The head's value of `key`.
  pub(crate) fn head(&self, key: &K) -> Option<&V> {
    self.keys.get(key)?.last()?.value.as_ref()
  }

  /// Whether the head holds `key`.
  pub(crate) fn head_has(&self, key: &K) -> bool {
    self.head(key).is_some()
  }

  /// The value of `key` a snapshot frozen at `epoch` holds.
  pub(crate) fn at(&self, key: &K, epoch: Epoch) -> Option<&V> {
    self
      .keys
      .get(key)?
      .iter()
      .rev()
      .find(|version| version.from <= epoch)?
      .value
      .as_ref()
  }

  /// Records `value` for `key` as the head's, at the head's epoch `head`, where `newest` is the epoch of
  /// the newest live snapshot (`None`: there is none); `None` removes it. The key's last version is
  /// overwritten when no live snapshot reads it, so the table grows only by what a snapshot still needs. A
  /// key with nothing but removals hides nothing, so it is dropped.
  pub(crate) fn set(&mut self, key: K, value: Option<V>, head: Epoch, newest: Option<Epoch>) {
    let versions = self.keys.entry(key.clone()).or_default();
    match versions.last_mut() {
      Some(last) if newest.is_none_or(|snapshot| last.from > snapshot) => last.value = value,
      Some(last) if last.value == value => {}
      _ => versions.push(Version { from: head, value }),
    }
    if versions.iter().all(|version| version.value.is_none()) {
      self.keys.remove(&key);
    }
  }

  /// Removes the head's value of `key`, at the head's epoch `head` (`newest` as for [`Self::set`]).
  pub(crate) fn remove(&mut self, key: &K, head: Epoch, newest: Option<Epoch>) {
    if self.keys.contains_key(key) {
      self.set(key.clone(), None, head, newest);
    }
  }

  /// Every key the head holds, with its value, in key order.
  pub(crate) fn iter_head(&self) -> impl Iterator<Item = (&K, &V)> {
    self
      .keys
      .iter()
      .filter_map(|(key, versions)| versions.last()?.value.as_ref().map(|value| (key, value)))
  }

  /// Keeps, per key, the head's version and the last version at or before each of `live` (the epochs of
  /// the snapshots that remain), and drops a key left with nothing but removals.
  pub(crate) fn prune(&mut self, live: &[Epoch]) {
    self.keys.retain(|_, versions| {
      let mut keep = vec![false; versions.len()];
      if let Some(head) = keep.last_mut() {
        *head = true;
      }
      for epoch in live {
        if let Some(index) = versions.iter().rposition(|version| version.from <= *epoch)
          && let Some(kept) = keep.get_mut(index)
        {
          *kept = true;
        }
      }
      let mut kept = keep.into_iter();
      versions.retain(|_| kept.next().unwrap_or(true));
      versions.iter().any(|version| version.value.is_some())
    });
  }

  /// The table a clone made from a snapshot frozen at `epoch` starts with: each key's value there, as one
  /// version from `epoch` (the clone's own epochs all come after it).
  pub(crate) fn view_at(&self, epoch: Epoch) -> Self {
    let keys = self
      .keys
      .iter()
      .filter_map(|(key, versions)| {
        let value = versions
          .iter()
          .rev()
          .find(|version| version.from <= epoch)?
          .value
          .clone()?;
        Some((
          key.clone(),
          vec![Version {
            from: epoch,
            value: Some(value),
          }],
        ))
      })
      .collect();
    Self { keys }
  }

  /// Every version of every key, in key order and then epoch order (the recovery image).
  pub(crate) fn versions(&self) -> impl Iterator<Item = (&K, &Version<V>)> {
    self
      .keys
      .iter()
      .flat_map(|(key, versions)| versions.iter().map(move |version| (key, version)))
  }

  /// Appends a recorded version of `key` (the recovery image): refused unless it comes after the key's
  /// last version, so a reordered or duplicated record is never taken for history.
  pub(crate) fn restore(&mut self, key: K, version: Version<V>) -> Result<(), RestoreRefusal> {
    let versions = self.keys.entry(key).or_default();
    if versions
      .last()
      .is_some_and(|last| last.from >= version.from)
    {
      return Err(RestoreRefusal);
    }
    versions.push(version);
    Ok(())
  }

  /// The versions held, over every key: the measure of the bound.
  pub(crate) fn version_count(&self) -> usize {
    self.keys.values().map(Vec::len).sum()
  }
}

/// A recorded version out of order: the image does not describe a history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RestoreRefusal;

#[cfg(test)]
mod tests {
  // proptest's strategy types carry `Arc` (D-8's harness exception).
  #![allow(clippy::disallowed_types)]

  use std::collections::BTreeMap;

  use proptest::prelude::*;

  use super::{Version, Versioned};
  use crate::ids::Epoch;

  /// Shape: the keys a generated history touches: few, so histories revisit them.
  const KEYS: u8 = 4;

  /// One step of a generated history.
  #[derive(Clone, Debug)]
  enum Step {
    Set(u8, u8),
    Remove(u8),
    Snapshot,
    Destroy(usize),
  }

  fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
      4 => (0..KEYS, any::<u8>()).prop_map(|(k, v)| Step::Set(k, v)),
      2 => (0..KEYS).prop_map(Step::Remove),
      2 => Just(Step::Snapshot),
      1 => any::<usize>().prop_map(Step::Destroy),
    ]
  }

  /// The model: the head's map, and each live snapshot's frozen copy with its epoch.
  #[derive(Default)]
  struct Model {
    head: BTreeMap<u8, u8>,
    snapshots: Vec<(Epoch, BTreeMap<u8, u8>)>,
    epoch: Epoch,
  }

  impl Model {
    /// The newest live snapshot's epoch.
    fn newest(&self) -> Option<Epoch> {
      self.snapshots.iter().map(|(epoch, _)| *epoch).max()
    }
  }

  /// Applies one step to the table and the model alike.
  fn apply(step: &Step, table: &mut Versioned<u8, u8>, model: &mut Model) {
    match step {
      Step::Set(key, value) => {
        table.set(*key, Some(*value), model.epoch, model.newest());
        model.head.insert(*key, *value);
      }
      Step::Remove(key) => {
        table.remove(key, model.epoch, model.newest());
        model.head.remove(key);
      }
      Step::Snapshot => {
        model.snapshots.push((model.epoch, model.head.clone()));
        model.epoch = model.epoch.next();
      }
      Step::Destroy(pick) => {
        if !model.snapshots.is_empty() {
          model.snapshots.remove(pick % model.snapshots.len());
          let live: Vec<Epoch> = model.snapshots.iter().map(|(epoch, _)| *epoch).collect();
          table.prune(&live);
        }
      }
    }
  }

  proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, failure_persistence: None, .. ProptestConfig::default() })]

    /// A-48: over any history of writes, removals, snapshots and snapshot destroys, the head reads the
    /// model's head, every live snapshot reads exactly what the model froze for it, and after every step no
    /// key holds more versions than the live snapshots plus one.
    #[test]
    fn every_snapshot_reads_what_it_froze_within_the_bound(
      steps in proptest::collection::vec(step(), 0..64),
    ) {
      let mut table = Versioned::default();
      let mut model = Model::default();
      for step in &steps {
        apply(step, &mut table, &mut model);
        for key in 0..KEYS {
          prop_assert_eq!(table.head(&key), model.head.get(&key));
          for (epoch, frozen) in &model.snapshots {
            prop_assert_eq!(table.at(&key, *epoch), frozen.get(&key), "key {} at {:?}", key, epoch);
          }
        }
        let bound = usize::from(KEYS) * (model.snapshots.len() + 1);
        prop_assert!(table.version_count() <= bound, "{} versions over a bound of {}", table.version_count(), bound);
      }
    }

    /// A-48: a clone made from a snapshot starts with that snapshot's values, not the head's.
    #[test]
    fn a_clone_starts_from_its_snapshots_values(
      steps in proptest::collection::vec(step(), 0..64),
    ) {
      let mut table = Versioned::default();
      let mut model = Model::default();
      for step in &steps {
        apply(step, &mut table, &mut model);
      }
      for (epoch, frozen) in &model.snapshots {
        let clone = table.view_at(*epoch);
        let seen: BTreeMap<u8, u8> = clone.iter_head().map(|(key, value)| (*key, *value)).collect();
        prop_assert_eq!(&seen, frozen);
      }
    }
  }

  /// A-48 (the recovery image): versions restore in order and a reordered one is refused.
  #[test]
  fn a_restored_history_must_run_forward() {
    let mut table: Versioned<u8, u8> = Versioned::default();
    table
      .restore(
        1,
        Version {
          from: Epoch(2),
          value: Some(7),
        },
      )
      .unwrap();
    assert!(
      table
        .restore(
          1,
          Version {
            from: Epoch(2),
            value: None
          }
        )
        .is_err(),
      "a version at the same epoch is not history"
    );
    table
      .restore(
        1,
        Version {
          from: Epoch(5),
          value: None,
        },
      )
      .unwrap();
    assert_eq!(table.at(&1, Epoch(3)), Some(&7));
    assert_eq!(table.head(&1), None);
  }
}
