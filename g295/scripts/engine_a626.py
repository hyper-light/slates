import sys
p=sys.argv[1]
s=open(p).read()
def rep(old,new,count=1):
    global s
    n=s.count(old)
    if n!=count:
        raise SystemExit(f"anchor count {n} != {count}: {old[:80]!r}")
    s=s.replace(old,new)

# (a) last_version -> Recorded + last_entry
rep('''/// The version of a history's last entry: `None` for a key with no history.
fn last_version<T>(history: Option<&Vec<(u64, T)>>) -> Option<u64> {
  history
    .and_then(|history| history.last())
    .map(|(version, _)| *version)
}
''','''/// What a history's entry records, as the copy rule reads it ([`Green::covered_since`], A-626): whether the entry is
/// the key's removal.
trait Recorded {
  /// Whether the entry records the key's removal.
  fn removed(&self) -> bool;
}

impl<T> Recorded for Option<T> {
  fn removed(&self) -> bool {
    self.is_none()
  }
}

impl Recorded for bool {
  fn removed(&self) -> bool {
    !*self
  }
}

/// A history's last entry: its version, and whether it records the key's removal. `None` for a key with no history.
fn last_entry<T: Recorded>(history: Option<&Vec<(u64, T)>>) -> Option<(u64, bool)> {
  history
    .and_then(|history| history.last())
    .map(|(version, value)| (*version, value.removed()))
}
''')
s=s.replace('last_version(','last_entry(')

# (b) Resolved.copies
rep('''  /// Each copy's destination, with its source and the source's version (A-586).
  copies: BTreeMap<String, (String, u64)>,''','''  /// Each copy's destination, with what it copies (A-586, A-626).
  copies: BTreeMap<String, copy::CopySpec>,''')

# (c) declare_copy
rep('''  /// Notes a copy of `src` at `version` to `dst` (A-586); a second copy onto one destination in one increment is no copy
  /// anyone can mean, so the destination is invalid.
  fn declare_copy(&mut self, dst: String, src: String, version: u64) {
    if self.copies.insert(dst.clone(), (src, version)).is_some() {''','''  /// Notes a copy to `dst` (A-586); a second copy onto one destination in one increment is no copy anyone can mean, so
  /// the destination is invalid.
  fn declare_copy(&mut self, dst: String, spec: copy::CopySpec) {
    if self.copies.insert(dst.clone(), spec).is_some() {''')

# (d) Effect::CopyTree
rep('''  /// Copy a tree by reference (A-586): its destination, its source and the source's version.
  CopyTree(String, String, u64),''','''  /// Copy a tree by reference (A-586): its destination, its source, the source's version, and whether it replaces what
  /// the destination holds (A-626).
  CopyTree(String, String, u64, bool),''')

# (e) applied()
rep('''      Effect::CopyTree(dst, from, version) => Applied::CopyTree {
        dst: dst.clone(),
        from: from.clone(),
        version: *version,
      },''','''      Effect::CopyTree(dst, from, version, replace) => Applied::CopyTree {
        dst: dst.clone(),
        from: from.clone(),
        version: *version,
        replace: *replace,
      },''')

# (g) resolve arm
rep('''        OpKind::Copy => r.declare_copy(path, name(op.src), op.at),''','''        OpKind::Copy => r.declare_copy(
          path,
          copy::CopySpec {
            source: name(op.src),
            version: op.at,
            replace: op.replaces(),
          },
        ),''')

# (h) effect_of
rep('''      Applied::CopyTree { dst, from, version } => {
        Effect::CopyTree(dst.clone(), from.clone(), *version)
      }''','''      Applied::CopyTree {
        dst,
        from,
        version,
        replace,
      } => Effect::CopyTree(dst.clone(), from.clone(), *version, *replace),''')

# (i) head_identity_from_scratch split
rep('''  pub fn head_identity_from_scratch(&self) -> [u8; 32] {
    let mut set = crate::identity::SetHash::default();
    for (path, node) in self.specials.iter() {''','''  pub fn head_identity_from_scratch(&self) -> [u8; 32] {
    let mut set = crate::identity::SetHash::default();
    self.add_state_entries(&mut set);
    // Each copy's entry in, and the entries of the tree it copied out: the running sum adds the one and never the
    // others (A-586). The recount reads each source at its version, so it holds while the histories reach that far.
    for (dst, records) in &self.copies {
      for record in records {
        set.add(&copy_entry(dst, &record.source, record.source_version));
        for entry in self.copied_tree_entries(dst, record) {
          set.remove(&entry);
        }
      }
    }
    set.identity()
  }

  /// Every entry of the current state, added to `set` ([`Green::head_identity_from_scratch`]).
  fn add_state_entries(&self, set: &mut crate::identity::SetHash) {
    for (path, node) in self.specials.iter() {''')
rep('''    for ((path, name), value) in self.xattrs.iter() {
      set.add(&xattr_entry(&path, &name, value));
    }
    // Each copy's entry in, and the entries of the tree it copied out: the running sum adds the one and never the
    // others (A-586). The recount reads each source at its version, so it holds while the histories reach that far.
    for (dst, records) in &self.copies {
      for record in records {
        set.add(&copy_entry(dst, &record.source, record.source_version));
        for entry in self.copied_tree_entries(dst, record) {
          set.remove(&entry);
        }
      }
    }
    set.identity()
  }''','''    for ((path, name), value) in self.xattrs.iter() {
      set.add(&xattr_entry(&path, &name, value));
    }
  }''')

# (j) Staged
rep('''      Effect::SetContent(path, bytes, ops, source) => {
        let renamed = matches!(source, ContentSource::Copied(..));
        staged.set.push((path, bytes, ops, renamed));
      }''','''      Effect::SetContent(path, bytes, ops, source) => staged.set.push(StagedFile {
        path,
        bytes,
        ops,
        renamed: matches!(source, ContentSource::Copied(..)),
      }),''')
rep('''    let mut delta: BTreeMap<String, Vec<Op>> = BTreeMap::new();
    for (path, bytes, ops, renamed) in staged.set {
      self.set_file(path, (bytes, ops, renamed), version, &mut delta);
    }''','''    let mut delta: BTreeMap<String, Vec<Op>> = BTreeMap::new();
    for file in staged.set {
      self.set_file(file, version, &mut delta);
    }''')
rep('''  /// Sets the file at `path` to new bytes at `version` (a staged set), its delta recorded in `delta`.
  fn set_file(
    &mut self,
    path: String,
    (bytes, ops, renamed): (Vec<u8>, Vec<Op>, bool),
    version: u64,
    delta: &mut BTreeMap<String, Vec<Op>>,
  ) {''','''  /// Sets a staged file to its new bytes at `version`, its delta recorded in `delta`.
  fn set_file(&mut self, file: StagedFile, version: u64, delta: &mut BTreeMap<String, Vec<Op>>) {
    let StagedFile {
      path,
      bytes,
      ops,
      renamed,
    } = file;''')
# Staged types before the `Effect` enum
rep('''/// One accepted per-path effect to apply at commit.
enum Effect {''','''/// What a commit applies once every other effect has landed ([`Green::commit`]): the files it removes, then the files it
/// sets. Removals come first, so a rename's source removal never deletes a file the same increment recreates or renames
/// into at that path.
#[derive(Default)]
struct Staged {
  removed: Vec<String>,
  set: Vec<StagedFile>,
}

/// A file's new bytes, staged ([`Staged`]): with the ops that make them (head coordinates), and whether a rename or a
/// copied tree's edit brought them (`ContentSource::Copied`), which A-612's rule reads.
struct StagedFile {
  path: String,
  bytes: Vec<u8>,
  ops: Vec<Op>,
  renamed: bool,
}

/// One accepted per-path effect to apply at commit.
enum Effect {''')

# (k) reservations
rep('''    indexed.saturating_add(Green::copy_records_reservation(inc))
  }''','''    indexed
      .saturating_add(Green::copy_records_reservation(inc))
      .saturating_add(self.replace_changes_reservation(inc))
  }''')
rep('''    replaced.into_iter().fold(content, |total, path| {
      total
        .saturating_add(self.latest_bytes(path))
        .saturating_add(if self.specials.contains_key(path) {
          SPECIAL_NODE_BYTES
        } else {
          0
        })
    })
  }''','''    let replaced = replaced.into_iter().fold(content, |total, path| {
      total
        .saturating_add(self.latest_bytes(path))
        .saturating_add(if self.specials.contains_key(path) {
          SPECIAL_NODE_BYTES
        } else {
          0
        })
    });
    replaced.saturating_add(self.replace_history_reservation(inc))
  }''')
open(p,'w').write(s)
print("ok")
