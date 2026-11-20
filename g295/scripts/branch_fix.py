import sys
p=sys.argv[1]
s=open(p).read()
def rep(old,new):
    global s
    assert s.count(old)==1, old[:80]
    s=s.replace(old,new)
# imports
rep('''  history_at, last_entry, link_entry, mode_entry, present_at, relink, special_entry, xattr_entry,''',
'''  last_entry, link_entry, mode_entry, relink, special_entry, xattr_entry,''')
# changed_since -> set_back
start=s.index("/// The value a branch sets a key back to, when it differs from the head's:")
end=s.index("/// Moves `identity` from an entry to another")
s=s[:start]+'''/// What a branch does to one key (module docs): its value at the version (`then`, read as the green's readers read it)
/// against the head's (`now`); the identity's entry exchanged when they differ, and the key set back when `then`
/// differs from what the branch holds once its cuts are made, nothing for a key in a tree a cut took away (`cut`). The
/// value to set back, when one is owed.
fn set_back<T: PartialEq>(
  identity: &mut SetHash,
  (then, now): (Option<T>, Option<&T>),
  cut: bool,
  entry: impl Fn(&T) -> Entry,
) -> Option<Option<T>> {
  if then.as_ref() != now {
    exchange(identity, now.map(&entry), then.as_ref().map(&entry));
  }
  let held = now.filter(|_| !cut);
  (then.as_ref() != held).then_some(then)
}

'''+s[end:]
# the reverts loop passes whether a cut took the path away
rep('''    for path in &changed {
      self.file_reverts(path, version, identity, &mut reverts);
      self.node_reverts(path, version, identity, &mut reverts);
      self.link_reverts(path, version, identity, &mut reverts);
      self.xattr_reverts(path, version, identity, &mut reverts);''','''    for path in &changed {
      // A copy made after the version takes its whole tree away, keys equal at both ends among them (A-626).
      let cut = !self.copies_above_after(path, version).is_empty();
      self.file_reverts(path, (version, cut), identity, &mut reverts);
      self.node_reverts(path, (version, cut), identity, &mut reverts);
      self.link_reverts(path, (version, cut), identity, &mut reverts);
      self.xattr_reverts(path, (version, cut), identity, &mut reverts);''')
# file_reverts
rep('''  /// [`Green::reverts`] of `path`'s bytes.
  fn file_reverts(
    &self,
    path: &str,
    version: u64,
    identity: &mut SetHash,
    reverts: &mut Vec<Revert>,
  ) {
    if !self.content_history.contains_key(path) {
      return;
    }
    let then = self.content_ref_at(path, version);
    let now = self.content.get(path);
    if then.as_deref() != now {
      exchange(
        identity,
        now.map(|bytes| file_entry(path, bytes)),
        then.as_deref().map(|bytes| file_entry(path, bytes)),
      );
      reverts.push(Revert::File(path.to_owned(), then.map(Cow::into_owned)));
    }
  }''','''  /// [`Green::reverts`] of `path`'s bytes. `cut`: a copy made after `version` took `path`'s tree away.
  fn file_reverts(
    &self,
    path: &str,
    (version, cut): (u64, bool),
    identity: &mut SetHash,
    reverts: &mut Vec<Revert>,
  ) {
    if !self.content_history.contains_key(path) {
      return;
    }
    let then = self.content_ref_at(path, version);
    let now = self.content.get(path);
    if then.as_deref() != now {
      exchange(
        identity,
        now.map(|bytes| file_entry(path, bytes)),
        then.as_deref().map(|bytes| file_entry(path, bytes)),
      );
    }
    if then.as_deref() != now.filter(|_| !cut) {
      reverts.push(Revert::File(path.to_owned(), then.map(Cow::into_owned)));
    }
  }''')
# node_reverts
a=s.index("  /// [`Green::reverts`] of `path`'s IPC node, directory and mode.")
b=s.index("  /// [`Green::reverts`] of `path`'s symbolic link and hard link.")
s=s[:a]+'''  /// [`Green::reverts`] of `path`'s IPC node, directory and mode.
  fn node_reverts(
    &self,
    path: &str,
    (version, cut): (u64, bool),
    identity: &mut SetHash,
    reverts: &mut Vec<Revert>,
  ) {
    if self.special_history.contains_key(path) {
      let pair = (self.special_at(path, version), self.specials.get(path));
      if let Some(then) = set_back(identity, pair, cut, |node| special_entry(path, node)) {
        reverts.push(Revert::Special(path.to_owned(), then));
      }
    }
    if self.dir_history.contains_key(path) {
      let (then, now) = (self.dir_at(path, version), self.dirs.contains_key(path));
      if then != now {
        exchange(
          identity,
          now.then(|| dir_entry(path)),
          then.then(|| dir_entry(path)),
        );
      }
      if then != (now && !cut) {
        reverts.push(Revert::Dir(path.to_owned(), then));
      }
    }
    if self.mode_history.contains_key(path) {
      let pair = (self.mode_at(path, version), self.modes.get(path));
      if let Some(then) = set_back(identity, pair, cut, |mode| mode_entry(path, *mode)) {
        reverts.push(Revert::Mode(path.to_owned(), then));
      }
    }
  }

'''+s[b:]
# link_reverts
a=s.index("  /// [`Green::reverts`] of `path`'s symbolic link and hard link.")
b=s.index("  /// [`Green::reverts`] of `path`'s attributes: every name its attribute history holds.")
s=s[:a]+'''  /// [`Green::reverts`] of `path`'s symbolic link and hard link.
  fn link_reverts(
    &self,
    path: &str,
    (version, cut): (u64, bool),
    identity: &mut SetHash,
    reverts: &mut Vec<Revert>,
  ) {
    let symlink = |target: &String| link_entry(IDENTITY_TAG_SYMLINK, path, target);
    if self.symlink_history.contains_key(path) {
      let pair = (self.symlink_at(path, version), self.symlinks.get(path));
      if let Some(then) = set_back(identity, pair, cut, symlink) {
        reverts.push(Revert::Symlink(path.to_owned(), then));
      }
    }
    let hardlink = |target: &String| link_entry(IDENTITY_TAG_HARDLINK, path, target);
    if self.hardlink_history.contains_key(path) {
      let pair = (self.hardlink_at(path, version), self.hardlinks.get(path));
      if let Some(then) = set_back(identity, pair, cut, hardlink) {
        reverts.push(Revert::Hardlink(path.to_owned(), then));
      }
    }
  }

'''+s[b:]
# xattr_reverts
a=s.index("  /// [`Green::reverts`] of `path`'s attributes: every name its attribute history holds.")
b=s.index("  /// Sets one key back: the map's write, and the reverse map of hard links with a link's.")
s=s[:a]+'''  /// [`Green::reverts`] of `path`'s attributes: every name its attribute history holds.
  fn xattr_reverts(
    &self,
    path: &str,
    (version, cut): (u64, bool),
    identity: &mut SetHash,
    reverts: &mut Vec<Revert>,
  ) {
    let from = (path.to_owned(), String::new());
    for ((_, name), _) in self
      .xattr_history
      .range(from..)
      .take_while(|((at, _), _)| at == path)
    {
      let pair = (self.xattr_at(path, name, version), self.xattrs.get(path, name));
      let entry = |value: &Vec<u8>| xattr_entry(path, name, value);
      if let Some(then) = set_back(identity, pair, cut, entry) {
        reverts.push(Revert::Xattr(path.to_owned(), name.clone(), then));
      }
    }
  }

'''+s[b:]
open(p,'w').write(s)
print("ok")
