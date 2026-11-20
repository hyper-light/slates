import sys
p=sys.argv[1]
s=open(p).read()
start=s.index("  /// Remove the live inode and its metadata while retaining their historical values.\n  fn remove_special(")
end=s.index("  /// Records what version `version` changed in the change index (`changes`): every path an effect changed, and every")
new='''  /// Remove the live inode and its metadata while retaining their historical values.
  fn remove_special(&mut self, path: String, version: u64) {
    // The inode's own key is marked by its effect; its mode and attributes go with it and are marked here.
    let at = self.seed_of(&self.special_history, &path, &path, version);
    seed(
      &mut self.special_history,
      &path,
      held(self.specials.get(&path)),
      at,
    );
    if self.specials.remove(&path).is_some() {
      self.special_history_bytes = self
        .special_history_bytes
        .saturating_add(SPECIAL_NODE_BYTES);
    }
    self.special_changed.insert(path.clone(), version);
    self.remove_mode(&path, version);
    let attributes: Vec<(String, Vec<u8>)> = self
      .xattrs
      .of(&path)
      .map(|(name, value)| (name, value.clone()))
      .collect();
    for (name, value) in attributes {
      self.xattrs.remove(&path, &name);
      let key = (path.clone(), name);
      let at = self.seed_of(&self.xattr_history, &key, &path, version);
      seed(&mut self.xattr_history, &key, Some(Some(value)), at);
      self
        .identity_dirty
        .insert(IdentityKey::Xattr(key.0.clone(), key.1.clone()));
      self.xattr_changed.insert(key.clone(), version);
      self
        .xattr_history
        .entry(key)
        .or_default()
        .push((version, None));
    }
    self
      .special_history
      .entry(path)
      .or_default()
      .push((version, None));
  }

  /// Removes `path`'s mode at `version`, recorded as a removal (an IPC node's going, a replaced tree's, A-626): nothing
  /// for a path with none.
  fn remove_mode(&mut self, path: &str, version: u64) {
    let key = path.to_owned();
    let at = self.seed_of(&self.mode_history, &key, path, version);
    seed(&mut self.mode_history, &key, held(self.modes.get(path)), at);
    if self.modes.remove(path).is_some() {
      self.identity_dirty.insert(IdentityKey::Mode(key.clone()));
      self.mode_changed.insert(key.clone(), version);
      self
        .mode_history
        .entry(key)
        .or_default()
        .push((version, None));
    }
  }

  /// The [`Seed`] of a change at `version` to `key` of `history`, on `path`: the version the key's held value dates
  /// from, where its history does not hold it already (A-534's one history rule, A-586's copies).
  fn seed_of<K: Ord, T: Recorded>(
    &self,
    history: &BTreeMap<K, Vec<(u64, T)>>,
    key: &K,
    path: &str,
    version: u64,
  ) -> Seed {
    Seed {
      since: self.seed_since(path, last_entry(history.get(key))),
      version,
    }
  }

  /// Applies the accepted effects, committing version `version`: the copies first (`commit_copies`, A-586 and A-626),
  /// so what the version does to a copied tree lands on what the copy brought, then every other effect, the content's
  /// removals and sets last.
  fn commit(&mut self, effects: Vec<Effect>, version: u64) {
    let mut changed: BTreeSet<String> = effects.iter().map(|e| e.path().to_owned()).collect();
    changed.extend(self.commit_copies(&effects, version));
    let mut staged = Staged::default();
    for effect in effects {
      self.apply_effect(effect, version, &mut staged);
    }
    for path in staged.removed {
      self.remove_file_now(&path, version);
    }
    let mut delta: BTreeMap<String, Vec<Op>> = BTreeMap::new();
    for (path, bytes, ops, renamed) in staged.set {
      self.set_file(path, (bytes, ops, renamed), version, &mut delta);
    }
    self.deltas.push(delta);
    self.record_changes(version, changed);
  }

  /// Applies one accepted effect at `version`, but a file's removal or new bytes, which are staged for the end of the
  /// commit ([`Staged`]), and a copy, which `commit_copies` applied first.
  fn apply_effect(&mut self, effect: Effect, version: u64, staged: &mut Staged) {
    // Every key an effect touches is marked once; the identity is brought up to date only when it is read (A-202).
    self.identity_dirty.insert(effect.identity_key());
    match effect {
      Effect::Special(path, node) => self.commit_special(path, node, version),
      Effect::RemoveSpecial(path) => self.remove_special(path, version),
      Effect::SetContent(path, bytes, ops, source) => {
        let renamed = matches!(source, ContentSource::Copied(..));
        staged.set.push((path, bytes, ops, renamed));
      }
      Effect::RemoveContent(path) => staged.removed.push(path),
      // Each key's history first records the value it held when its history does not hold it (`seed`: since version
      // 0 by the one history rule, A-534; since a copy, A-586), then its change.
      Effect::MakeDir(path) => {
        let stood = self.dirs.contains_key(&path).then_some(true);
        let at = self.seed_of(&self.dir_history, &path, &path, version);
        seed(&mut self.dir_history, &path, stood, at);
        self.dirs.insert(&path, ());
        self
          .dir_history
          .entry(path)
          .or_default()
          .push((version, true));
      }
      Effect::RemoveDir(path) => {
        let stood = self.dirs.contains_key(&path).then_some(true);
        let at = self.seed_of(&self.dir_history, &path, &path, version);
        seed(&mut self.dir_history, &path, stood, at);
        self.dirs.remove(&path);
        self
          .dir_history
          .entry(path)
          .or_default()
          .push((version, false));
      }
      Effect::Mode(path, mode) => {
        let at = self.seed_of(&self.mode_history, &path, &path, version);
        seed(
          &mut self.mode_history,
          &path,
          held(self.modes.get(&path)),
          at,
        );
        self.modes.insert(&path, mode);
        self.mode_changed.insert(path.clone(), version);
        self
          .mode_history
          .entry(path)
          .or_default()
          .push((version, Some(mode)));
      }
      Effect::Symlink(path, target) => self.set_symlink(path, Some(target), version),
      Effect::RemoveSymlink(path) => self.set_symlink(path, None, version),
      Effect::Hardlink(path, target) => self.set_hardlink(path, Some(target), version),
      Effect::RemoveHardlink(path) => self.set_hardlink(path, None, version),
      Effect::SetXattr(path, name, value) => self.set_xattr((path, name), Some(value), version),
      Effect::RemoveXattr(path, name) => self.set_xattr((path, name), None, version),
      // Applied before every other effect (`commit_copies`), so the effects on its tree land on what it brought.
      Effect::CopyTree(..) => {}
    }
  }

  /// Sets or removes (`None`) a symbolic link at `version`, its history recording the change.
  fn set_symlink(&mut self, path: String, target: Option<String>, version: u64) {
    let at = self.seed_of(&self.symlink_history, &path, &path, version);
    seed(
      &mut self.symlink_history,
      &path,
      held(self.symlinks.get(&path)),
      at,
    );
    match &target {
      Some(target) => {
        self.symlinks.insert(&path, target.clone());
      }
      None => {
        self.symlinks.remove(&path);
      }
    }
    self.symlink_changed.insert(path.clone(), version);
    self
      .symlink_history
      .entry(path)
      .or_default()
      .push((version, target));
  }

  /// Sets or removes (`None`) a hard link at `version`, the reverse map of links with it and its history recording the
  /// change.
  fn set_hardlink(&mut self, path: String, target: Option<String>, version: u64) {
    let at = self.seed_of(&self.hardlink_history, &path, &path, version);
    seed(
      &mut self.hardlink_history,
      &path,
      held(self.hardlinks.get(&path)),
      at,
    );
    let old = match &target {
      Some(target) => self.hardlinks.insert(&path, target.clone()),
      None => self.hardlinks.remove(&path),
    };
    relink(&mut self.links_to, &path, old.as_deref(), target.as_deref());
    self.hardlink_changed.insert(path.clone(), version);
    self
      .hardlink_history
      .entry(path)
      .or_default()
      .push((version, target));
  }

  /// Sets or removes (`None`) an attribute at `version`, its history recording the change.
  fn set_xattr(&mut self, key: (String, String), value: Option<Vec<u8>>, version: u64) {
    let before = match &value {
      Some(value) => self.xattrs.insert(&key.0, &key.1, value.clone()),
      None => self.xattrs.remove(&key.0, &key.1),
    }
    .map(Some);
    let at = self.seed_of(&self.xattr_history, &key, &key.0, version);
    seed(&mut self.xattr_history, &key, before, at);
    self.xattr_changed.insert(key.clone(), version);
    self
      .xattr_history
      .entry(key)
      .or_default()
      .push((version, value));
  }

  /// Removes the file at `path` at `version` (a staged removal, or a replaced tree's, A-626): its last value stays in
  /// the history for reconstruction, no longer a current value, so retained and charged (`history_bytes`).
  fn remove_file_now(&mut self, path: &str, version: u64) {
    self.identity_dirty.insert(IdentityKey::File(path.to_owned()));
    self.seed_content(path, version);
    self.content.remove(path);
    self.last_changed.insert(path.to_owned(), version);
    self.file_removed.insert(path.to_owned(), version);
    self.push_content(path.to_owned(), version, None);
  }

  /// Sets the file at `path` to new bytes at `version` (a staged set), its delta recorded in `delta`.
  fn set_file(
    &mut self,
    path: String,
    (bytes, ops, renamed): (Vec<u8>, Vec<Op>, bool),
    version: u64,
    delta: &mut BTreeMap<String, Vec<Op>>,
  ) {
    // A rename onto a file removes it (A-612); onto a path this commit emptied, the removal is recorded already. A file
    // this version's own copy brought and its increment edited (A-586) was no other file: nothing was removed.
    if renamed && self.content.contains_key(&path) && !self.copied_in(&path, version) {
      self.file_removed.insert(path.clone(), version);
    }
    // The value this one supersedes stays in the history, sharing every run the new one kept (W2): what the history
    // gains is the bytes the new value added, its piece list, and whatever the old one held that the new one does not,
    // the retention charged to the running totals.
    self.seed_content(&path, version);
    let value = self.content_value(&path, &ops, &bytes);
    self.content.insert(&path, bytes);
    self.last_changed.insert(path.clone(), version);
    self.push_content(path.clone(), version, Some(value));
    delta.insert(path, ops);
  }

'''
s=s[:start]+new+s[end:]
open(p,'w').write(s)
print("ok")
