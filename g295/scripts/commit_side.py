import sys
p=sys.argv[1]
s=open(p).read()
def rep(old,new):
    global s
    assert s.count(old)==1, old[:70]
    s=s.replace(old,new,1)
start=s.index("  /// Applies a copy the verdict accepted, at `version` (module docs): every map's copy (the reverse map of hard links has")
end=s.index("  /// The entries of the copies made at `dst` after `since`: what a copy puts into the set hash (module docs).")
new='''  /// Applies the copies among a version's accepted effects, in their destinations' order, an outer copy before one
  /// inside its tree (module docs): every source held first, as the version before this one, whatever a copy of this
  /// version takes away (FSFS copies from a revision, never a transaction); then each destination's tree taken away where
  /// the copy replaces it, and the held source grafted there. The paths a replace took away, for the change index.
  pub(super) fn commit_copies(&mut self, effects: &[Effect], version: u64) -> BTreeSet<String> {
    let mut copies: Vec<(&str, &str, u64, bool)> = effects
      .iter()
      .filter_map(|effect| match effect {
        Effect::CopyTree(dst, src, source_version, replace) => {
          Some((dst.as_str(), src.as_str(), *source_version, *replace))
        }
        _ => None,
      })
      .collect();
    copies.sort_by(|a, b| a.0.cmp(b.0));
    let held: Vec<HeldSource> = copies
      .iter()
      .map(|(_, src, ..)| self.hold_source(src))
      .collect();
    let mut removed = BTreeSet::new();
    for ((dst, src, source_version, replace), held) in copies.into_iter().zip(held) {
      if replace {
        removed.extend(self.clear_tree(dst, version));
      }
      self.graft_source(held, (dst, src, source_version), version);
    }
    removed
  }

  /// Every map's hold on `src`'s tree ([`HeldSource`]).
  fn hold_source(&mut self, src: &str) -> HeldSource {
    HeldSource {
      content: self.content.hold_tree(src),
      specials: self.specials.hold_tree(src),
      dirs: self.dirs.hold_tree(src),
      modes: self.modes.hold_tree(src),
      symlinks: self.symlinks.hold_tree(src),
      hardlinks: self.hardlinks.hold_tree(src),
      xattrs: self.xattrs.hold_tree(src),
    }
  }

  /// Grafts a held source at its copy's destination, at `version` (module docs): every map's graft (the reverse map of
  /// hard links has none: the source holds no link), and the copy's one record, `last_changed` entry and change-index
  /// bytes. A map that refuses what the verdict judged possible is counted ([`Green::copies_refused`]), never silently.
  fn graft_source(
    &mut self,
    held: HeldSource,
    (dst, src, source_version): (&str, &str, u64),
    version: u64,
  ) {
    let results = [
      self.content.graft_tree(held.content, dst),
      self.specials.graft_tree(held.specials, dst),
      self.dirs.graft_tree(held.dirs, dst),
      self.modes.graft_tree(held.modes, dst),
      self.symlinks.graft_tree(held.symlinks, dst),
      self.hardlinks.graft_tree(held.hardlinks, dst),
      self.xattrs.graft_tree(held.xattrs, dst),
    ];
    if results.iter().any(Result::is_err) {
      self.copies_refused = self.copies_refused.saturating_add(1);
    }
    if results.iter().flatten().any(|copied| copied.brought()) {
      self.copies_grafted = self.copies_grafted.saturating_add(1);
    }
    self.last_changed.insert(dst.to_owned(), version);
    self.changes_bytes = self
      .changes_bytes
      .saturating_add(appearance_bytes(dst, src));
    self
      .copies
      .entry(dst.to_owned())
      .or_default()
      .push(Appearance {
        version,
        source: src.to_owned(),
        source_version,
      });
  }

  /// Takes `dst`'s tree away key by key at `version`, a replace's first step (A-626): every file, IPC node, directory,
  /// mode, symbolic link and attribute in it, each recorded as its removal records it (its history seeded and closed, its
  /// last change, its identity entry marked; a file's removal for A-612's rule too). O(the tree), read by a walk: the
  /// replaced tree's cost, as removing it key by key costs, since every holder must answer its keys at the versions
  /// before. Every path it took away, for the change index.
  pub(super) fn clear_tree(&mut self, dst: &str, version: u64) -> Vec<String> {
    let tree = self.tree_keys(dst);
    for path in &tree.files {
      self.remove_file_now(path, version);
    }
    for path in &tree.specials {
      self.identity_dirty.insert(IdentityKey::Special(path.clone()));
      self.remove_special(path.clone(), version);
    }
    let mut staged = super::Staged::default();
    let removals: Vec<Effect> = tree
      .dirs
      .iter()
      .map(|path| Effect::RemoveDir(path.clone()))
      .chain(tree.symlinks.iter().map(|path| Effect::RemoveSymlink(path.clone())))
      .chain(
        tree
          .xattrs
          .iter()
          .map(|(path, name)| Effect::RemoveXattr(path.clone(), name.clone())),
      )
      .collect();
    for effect in removals {
      self.apply_effect(effect, version, &mut staged);
    }
    for path in &tree.modes {
      self.remove_mode(path, version);
    }
    tree.paths()
  }

  /// Every key in `dst`'s tree that a replace takes away, by dimension: one walk of each map beneath it, and its own.
  /// An IPC node's mode and attributes go with it ([`Green::remove_special`]), so they are left out of the rest.
  fn tree_keys(&self, dst: &str) -> TreeKeys {
    let prefix = beneath_prefix(dst);
    let specials = with_own(
      self.specials.prefix(&prefix).map(|(path, _)| path),
      dst,
      self.specials.contains_key(dst),
    );
    let special_paths: BTreeSet<String> = specials.iter().cloned().collect();
    let modes = with_own(
      self.modes.prefix(&prefix).map(|(path, _)| path),
      dst,
      self.modes.contains_key(dst),
    );
    TreeKeys {
      files: with_own(
        self.content.prefix(&prefix).map(|(path, _)| path),
        dst,
        self.content.contains_key(dst),
      ),
      dirs: with_own(
        self.dirs.prefix(&prefix).map(|(path, _)| path),
        dst,
        self.dirs.contains_key(dst),
      ),
      modes: modes
        .into_iter()
        .filter(|path| !special_paths.contains(path))
        .collect(),
      symlinks: with_own(
        self.symlinks.prefix(&prefix).map(|(path, _)| path),
        dst,
        self.symlinks.contains_key(dst),
      ),
      xattrs: self
        .xattrs
        .iter_tree(dst)
        .filter(|(path, _)| !special_paths.contains(path))
        .collect(),
      specials,
    }
  }

'''
s=s[:start]+new+s[end:]
rep('''/// Derived: the bytes one copy's record costs, charged with the change index (A-586):''','''/// Every map's hold on a copy's source tree (A-626): taken before the version changes any map, so every copy of one
/// version reads its source as the version before it.
struct HeldSource {
  content: Held<Block>,
  specials: Held<SpecialNode>,
  dirs: Held<()>,
  modes: Held<u32>,
  symlinks: Held<String>,
  hardlinks: Held<String>,
  xattrs: HeldAttributes,
}

/// The keys a replace takes away from a destination's tree, by dimension ([`Green::clear_tree`]).
struct TreeKeys {
  files: Vec<String>,
  specials: Vec<String>,
  dirs: Vec<String>,
  modes: Vec<String>,
  symlinks: Vec<String>,
  xattrs: Vec<(String, String)>,
}

impl TreeKeys {
  /// Every path a key of the tree is at, once each.
  fn paths(&self) -> Vec<String> {
    let paths: BTreeSet<&String> = self
      .files
      .iter()
      .chain(&self.specials)
      .chain(&self.dirs)
      .chain(&self.modes)
      .chain(&self.symlinks)
      .chain(self.xattrs.iter().map(|(path, _)| path))
      .collect();
    paths.into_iter().cloned().collect()
  }
}

/// The paths a walk beneath a tree answered, with the tree's own path when its own key is there.
fn with_own(beneath: impl Iterator<Item = String>, own: &str, held: bool) -> Vec<String> {
  let mut paths: Vec<String> = beneath.collect();
  if held {
    paths.push(own.to_owned());
  }
  paths
}

/// Derived: the bytes one copy's record costs, charged with the change index (A-586):''')
rep('''use crate::identity::Entry;
use crate::ops_doc::Op;
use crate::verdict::MergeConflictClass;''','''use crate::dims::{Block, Held, HeldAttributes};
use crate::identity::Entry;
use crate::ops_doc::Op;
use crate::special::SpecialNode;
use crate::verdict::MergeConflictClass;''')
open(p,'w').write(s)
print("ok")
