//! A checkpoint of a green holding copies (A-626 part 3; §4.16 "The state's maps", "Copies"; A-586, A-238). A copy is
//! recorded once, at its destination, and the keys of its tree hold what it gave them with no history of their own,
//! read by its record; and the identity's set hash holds one entry for it, standing for its tree's entries at the
//! source's version. A checkpoint at a version `F` at or after a copy therefore carries what a restore from the origin
//! and the changed keys could not rebuild (`crate::checkpoint`, its module docs):
//! - every copy's record made at or before `F`, which the restored green keeps as this one keeps them;
//! - each key of a copied tree that holds at `F` what the copy above it gave it (no change of its own after the copy at
//!   or before `F`), read at `F` as every reader reads it: the restore sets it with no history, so it reads by its
//!   record again, and it is left out of the changed keys, which list every other key whose value at `F` was set after
//!   the origin by its own change;
//! - the identity's running sum at `F`, moved there from the head by the keys changed since, as a branch moves it
//!   ([`Green::identity_moved_to`], `branch`), which the restore takes as its own: what a copy's entry stands for (its
//!   source's tree at the source's version) is read from histories a restored green no longer holds. What the sum
//!   holds beyond the restored state's own entries is kept beside it (`copies_before`), so the recount still equals
//!   the identity. Without a copy's record at or before `F` it holds nothing beyond them, so the checkpoint carries
//!   none and the restore keeps the identity its own entries make (A-626 part 3, continued (1)).
//!
//! A copy at or before `F` needs nothing more after the restore: its keys read their current values from its record
//! on, and the identity is past it. A copy the restored green replays after `F` may name a source version before `F`
//! (the owner judged it above its own fold floor); what it brought is read from its graft, the source at the version
//! before it with its fix, which its chain entry carries, never from the source's history at that version, which the
//! restored green does not hold (`Green::brought_entry`, the parent module's docs). The checkpoint's format is the same
//! either way: a copy's record carries no fix and no copied entry.
//!
//! **Cost.** Each copied tree at or before `F` is listed key by key, O(the trees' keys, and the files' bytes they hold),
//! where the green holds each tree by reference: the price of a checkpoint that a restore reads alone. Moving the sum to
//! `F` costs what a branch at `F` costs, the keys changed since `F`, and its 2,048 bytes. A green with no copy pays two
//! presence bytes and no sum: version 4 carried the sum in every checkpoint, and the service's doubling rule then held
//! a small green's first versions unfolded (`crate::checkpoint`'s module docs).

use std::collections::BTreeSet;

use super::copy::{Appearance, ancestors, appearance_bytes};
use super::{
  Green, IDENTITY_TAG_HARDLINK, IDENTITY_TAG_SYMLINK, IdentityKey, dir_entry, file_entry,
  link_entry, mode_entry, relink, special_entry, xattr_entry,
};
use crate::checkpoint::{Changed, CopyRecord, Sum, Tables};
use crate::identity::SetHash;
use crate::special::SpecialNode;

/// Whether `history` holds an entry recorded in `(after, at]`.
fn entry_in<T>(history: Option<&Vec<(u64, T)>>, (after, at): (u64, u64)) -> bool {
  history.is_some_and(|history| {
    history
      .iter()
      .any(|(version, _)| *version > after && *version <= at)
  })
}

/// Whether `history` holds an entry recorded at or before `at`.
fn entry_by<T>(history: Option<&Vec<(u64, T)>>, at: u64) -> bool {
  history.is_some_and(|history| history.iter().any(|(version, _)| *version <= at))
}

/// `table` without the keys the copied tables list (`listed`), each key named in the identity's terms by `key_of`.
fn unlisted<K, V>(
  table: Vec<Changed<K, V>>,
  listed: &BTreeSet<IdentityKey>,
  key_of: impl Fn(&K) -> IdentityKey,
) -> Vec<Changed<K, V>> {
  table
    .into_iter()
    .filter(|changed| !listed.contains(&key_of(&changed.key)))
    .collect()
}

impl Green {
  /// The copy records made at or before `at`, as a checkpoint carries them.
  pub(super) fn copy_records_at(&self, at: u64) -> Vec<CopyRecord> {
    self
      .copies
      .iter()
      .flat_map(|(destination, records)| {
        records
          .iter()
          .filter(move |record| record.version <= at)
          .map(move |record| CopyRecord {
            destination: destination.clone(),
            version: record.version,
            source: record.source.clone(),
            source_version: record.source_version,
          })
      })
      .collect()
  }

  /// The latest copy made at or before `at` whose tree holds `path`: of copies one version made, the nearest, as
  /// [`Green::copy_above`] answers at the head.
  fn copy_above_at(&self, path: &str, at: u64) -> Option<&Appearance> {
    let mut latest: Option<&Appearance> = None;
    for prefix in ancestors(path) {
      if let Some(record) = self
        .copies
        .get(prefix)
        .and_then(|records| records.iter().rev().find(|record| record.version <= at))
        && latest.is_none_or(|best| record.version > best.version)
      {
        latest = Some(record);
      }
    }
    latest
  }

  /// Whether `key`'s own history records a change in `(after, at]` (`entry_in`), or any at or before `at` (`by`).
  fn own_entries(&self, key: &IdentityKey, (after, at): (u64, u64)) -> (bool, bool) {
    fn both<T>(history: Option<&Vec<(u64, T)>>, span: (u64, u64)) -> (bool, bool) {
      (entry_in(history, span), entry_by(history, span.1))
    }
    let span = (after, at);
    match key {
      IdentityKey::File(path) => both(self.content_history.get(path), span),
      IdentityKey::Special(path) => both(self.special_history.get(path), span),
      IdentityKey::Dir(path) => both(self.dir_history.get(path), span),
      IdentityKey::Mode(path) => both(self.mode_history.get(path), span),
      IdentityKey::Symlink(path) => both(self.symlink_history.get(path), span),
      IdentityKey::Hardlink(path) => both(self.hardlink_history.get(path), span),
      IdentityKey::Xattr(path, name) => {
        both(self.xattr_history.get(&(path.clone(), name.clone())), span)
      }
      IdentityKey::Copy(_) => (false, false),
    }
  }

  /// The copied trees' keys holding at `at` what the copy above them gave them (the module docs): every key of every
  /// copied tree made at or before `at`, current or recorded, whose own history records no change after that copy and at
  /// or before `at`, read at `at` as every reader reads it, with the copy's version. A key absent at `at` is listed only
  /// where something stood at it at or before `at`, which a restore must take away. With the keys listed.
  pub(super) fn copied_at(&self, at: u64) -> (Tables, BTreeSet<IdentityKey>) {
    let mut keys: BTreeSet<IdentityKey> = BTreeSet::new();
    for (destination, records) in &self.copies {
      if records.iter().any(|record| record.version <= at) {
        keys.extend(self.every_key(destination));
      }
    }
    let mut tables = Tables::default();
    let mut listed = BTreeSet::new();
    for key in keys {
      let Some(copy) = self
        .copy_above_at(key.path(), at)
        .map(|record| record.version)
      else {
        continue;
      };
      let (changed_since, recorded) = self.own_entries(&key, (copy, at));
      if !changed_since && self.list_copied(&key, (copy, at), recorded, &mut tables) {
        listed.insert(key);
      }
    }
    (tables, listed)
  }

  /// Lists `key`'s value at `at` in its table of `tables`, with the version `copy` of the copy that gave it: present, or
  /// absent where its own history `recorded` something then. Whether it was listed.
  fn list_copied(
    &self,
    key: &IdentityKey,
    (copy, at): (u64, u64),
    recorded: bool,
    tables: &mut Tables,
  ) -> bool {
    fn put<K, V>(
      table: &mut Vec<Changed<K, V>>,
      entry: (K, u64, Option<V>),
      recorded: bool,
    ) -> bool {
      let (key, at, value) = entry;
      let listed = value.is_some() || recorded;
      if listed {
        table.push(Changed { key, at, value });
      }
      listed
    }
    match key {
      IdentityKey::File(path) => {
        let bytes = self
          .content_ref_at(path, at)
          .map(|bytes| bytes.into_owned());
        put(&mut tables.files, (path.clone(), copy, bytes), recorded)
      }
      IdentityKey::Special(path) => {
        let node = self.special_at(path, at);
        put(&mut tables.specials, (path.clone(), copy, node), recorded)
      }
      IdentityKey::Dir(path) => {
        let dir = self.dir_at(path, at).then_some(());
        put(&mut tables.dirs, (path.clone(), copy, dir), recorded)
      }
      IdentityKey::Mode(path) => {
        let mode = self.mode_at(path, at);
        put(&mut tables.modes, (path.clone(), copy, mode), recorded)
      }
      IdentityKey::Symlink(path) => {
        let target = self.symlink_at(path, at);
        put(&mut tables.symlinks, (path.clone(), copy, target), recorded)
      }
      IdentityKey::Hardlink(path) => {
        let target = self.hardlink_at(path, at);
        put(
          &mut tables.hardlinks,
          (path.clone(), copy, target),
          recorded,
        )
      }
      IdentityKey::Xattr(path, name) => {
        let value = self.xattr_at(path, name, at);
        let key = (path.clone(), name.clone());
        put(&mut tables.xattrs, (key, copy, value), recorded)
      }
      IdentityKey::Copy(_) => false,
    }
  }

  /// The checkpoint's changed tables without the keys `listed` as copied ([`Green::copied_at`]).
  pub(super) fn leave_out_copied(
    checkpoint: &mut crate::checkpoint::Checkpoint,
    listed: &BTreeSet<IdentityKey>,
  ) {
    let path = |key: &String| key.clone();
    checkpoint.files = unlisted(std::mem::take(&mut checkpoint.files), listed, |key| {
      IdentityKey::File(path(key))
    });
    checkpoint.dirs = unlisted(std::mem::take(&mut checkpoint.dirs), listed, |key| {
      IdentityKey::Dir(path(key))
    });
    checkpoint.modes = unlisted(std::mem::take(&mut checkpoint.modes), listed, |key| {
      IdentityKey::Mode(path(key))
    });
    checkpoint.symlinks = unlisted(std::mem::take(&mut checkpoint.symlinks), listed, |key| {
      IdentityKey::Symlink(path(key))
    });
    checkpoint.hardlinks = unlisted(std::mem::take(&mut checkpoint.hardlinks), listed, |key| {
      IdentityKey::Hardlink(path(key))
    });
    checkpoint.specials = unlisted(std::mem::take(&mut checkpoint.specials), listed, |key| {
      IdentityKey::Special(path(key))
    });
    checkpoint.xattrs = unlisted(
      std::mem::take(&mut checkpoint.xattrs),
      listed,
      |(path, name)| IdentityKey::Xattr(path.clone(), name.clone()),
    );
  }

  /// The identity's running sum at `at`, as a checkpoint carries it ([`Sum`]): the sum brought to the head by the keys
  /// changed since it was last read (on a copy of it, so the green is not changed), then moved to `at` by the keys
  /// changed since, as a branch at `at` moves it.
  pub(super) fn sum_at(&self, at: u64) -> Sum {
    let mut sum = self.identity.clone();
    for key in &self.identity_dirty {
      let (out, into) = self.identity_moves(key, self.identity_version);
      for entry in &out {
        sum.remove(entry);
      }
      for entry in &into {
        sum.add(entry);
      }
    }
    if at < self.head() {
      self.identity_moved_to(at, &mut sum);
    }
    Sum::new(sum.to_bytes())
  }

  /// Sets the copied trees' keys a checkpoint lists (`copied`) with no history, so each reads by its record, and keeps
  /// the checkpoint's copies' records (`copies`), each charged with the change index as a commit charges it.
  pub(super) fn restore_copied(&mut self, copied: &Tables, copies: &[CopyRecord]) {
    for changed in &copied.files {
      self.place_file(&changed.key, changed.value.as_deref());
    }
    for changed in &copied.dirs {
      self.place_dir(&changed.key, changed.value.is_some());
    }
    for changed in &copied.modes {
      self.place_mode(&changed.key, changed.value);
    }
    for changed in &copied.symlinks {
      self.place_link(IDENTITY_TAG_SYMLINK, &changed.key, changed.value.as_deref());
    }
    for changed in &copied.hardlinks {
      self.place_link(
        IDENTITY_TAG_HARDLINK,
        &changed.key,
        changed.value.as_deref(),
      );
    }
    for changed in &copied.specials {
      self.place_special(&changed.key, changed.value);
    }
    for changed in &copied.xattrs {
      let (path, name) = &changed.key;
      self.place_xattr(path, name, changed.value.as_deref());
    }
    for record in copies {
      self.restore_copy(record);
    }
  }

  /// Keeps one copy's record, as the commit that made it kept it: in its destination's records in version order, its
  /// destination's last change, and its bytes charged with the change index.
  fn restore_copy(&mut self, record: &CopyRecord) {
    let records = self.copies.entry(record.destination.clone()).or_default();
    let at = records
      .iter()
      .position(|kept| kept.version > record.version)
      .unwrap_or(records.len());
    // No fix: a copy at or before the checkpoint stands at or below the restored green's floor, where nothing reads what
    // it brought (`Green::release_kept_fixes`).
    records.insert(
      at,
      Appearance {
        version: record.version,
        source: record.source.clone(),
        source_version: record.source_version,
        fix: None,
      },
    );
    let last = self
      .last_changed
      .entry(record.destination.clone())
      .or_insert(record.version);
    *last = (*last).max(record.version);
    self.changes_bytes = self
      .changes_bytes
      .saturating_add(appearance_bytes(&record.destination, &record.source));
  }

  /// Takes a checkpoint's sum as the green's identity, keeping what it holds beyond the restored state's own entries
  /// (the copies' entries less their trees') for the recount (`copies_before`).
  pub(super) fn take_sum(&mut self, sum: &Sum) {
    let carried = SetHash::from_bytes(sum.bytes());
    let before = carried.less(&self.identity);
    self.copies_before = (!before.is_empty()).then_some(before);
    self.identity = carried;
  }

  /// A file's current bytes set or taken away, the identity's entry moved with them; no history.
  pub(super) fn place_file(&mut self, path: &str, bytes: Option<&[u8]>) {
    if let Some(old) = self.content.remove(path) {
      self.identity.remove(&file_entry(path, &old));
    }
    if let Some(bytes) = bytes {
      self.identity.add(&file_entry(path, bytes));
      self.content.insert(path, bytes.to_vec());
    }
  }

  /// A directory set or taken away, the identity's entry moved with it; no history.
  pub(super) fn place_dir(&mut self, path: &str, present: bool) {
    if self.dirs.remove(path).is_some() {
      self.identity.remove(&dir_entry(path));
    }
    if present {
      self.identity.add(&dir_entry(path));
      self.dirs.insert(path, ());
    }
  }

  /// A mode set or taken away, the identity's entry moved with it; no history.
  pub(super) fn place_mode(&mut self, path: &str, mode: Option<u32>) {
    if let Some(old) = self.modes.remove(path) {
      self.identity.remove(&mode_entry(path, old));
    }
    if let Some(mode) = mode {
      self.identity.add(&mode_entry(path, mode));
      self.modes.insert(path, mode);
    }
  }

  /// A symbolic or hard link (by `tag`) set or taken away, the identity's entry and the reverse map of hard links moved
  /// with it; no history.
  pub(super) fn place_link(&mut self, tag: &[u8], path: &str, target: Option<&str>) {
    let map = if tag == IDENTITY_TAG_SYMLINK {
      &mut self.symlinks
    } else {
      &mut self.hardlinks
    };
    let old = map.remove(path);
    if let Some(target) = target {
      map.insert(path, target.to_owned());
    }
    if let Some(old) = &old {
      self.identity.remove(&link_entry(tag, path, old));
    }
    if let Some(target) = target {
      self.identity.add(&link_entry(tag, path, target));
    }
    if tag != IDENTITY_TAG_SYMLINK {
      relink(&mut self.links_to, path, old.as_deref(), target);
    }
  }

  /// An IPC node set or taken away, the identity's entry moved with it; no history.
  pub(super) fn place_special(&mut self, path: &str, node: Option<SpecialNode>) {
    if let Some(old) = self.specials.remove(path) {
      self.identity.remove(&special_entry(path, &old));
    }
    if let Some(node) = node {
      self.identity.add(&special_entry(path, &node));
      self.specials.insert(path, node);
    }
  }

  /// An attribute set or taken away, the identity's entry moved with it; no history.
  pub(super) fn place_xattr(&mut self, path: &str, name: &str, value: Option<&[u8]>) {
    if let Some(old) = self.xattrs.remove(path, name) {
      self.identity.remove(&xattr_entry(path, name, &old));
    }
    if let Some(value) = value {
      self.identity.add(&xattr_entry(path, name, value));
      self.xattrs.insert(path, name, value.to_vec());
    }
  }
}
