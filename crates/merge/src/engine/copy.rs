//! A path's tree copied by reference in the merge engine (A-586, A-626; A-542's second half; §4.17.4 item 3 and piece
//! 3; §4.16 "The state's maps"): Subversion's copy, a branch or a tag made from any path at a version, which a stock
//! client's commit declares as an addition with a copy source, or as a replacement with one.
//!
//! **What a copy is.** `Copy` (`OpKind` 16) names its destination, its source and the source's version (Subversion's
//! copy-from revision: FSFS copies only from a committed revision, `libsvn_fs_fs/tree.c` `copy_helper`). The verdict
//! judges it node-exclusively, an addition: refused when the destination or anything beneath it is present at the head
//! (unless the copy replaces it, below) or changed after the increment's base, when the source's tree changed after the
//! version it is copied at, when
//! nothing is there to copy, when the source's tree holds a hard link (Subversion has none, and each would add one more
//! name to the reverse map of links, `links_to`), and when the destination is the root (always present; an editor
//! never adds it). Accepted, the head's maps copy the source's tree by reference (`crate::dims`, "A tree is copied by
//! reference"): since the source did not change after its version, the head's tree is that version's. The root is a
//! source like any other: its tree is every key (a stock client copies `^/` into a child, `libsvn_client/copy.c`
//! refusing a copy into its own child only between working copies).
//!
//! **One record a copy, never one a key.** A copy is recorded once, at its destination: an appearance record (the
//! version, the source, the source's version), one entry of the change index and one `last_changed` entry for the
//! destination. A key in a copied tree has no history of its own until it changes, so by A-534's one history rule it
//! would read as held since version 0; instead a key reads by the latest copy above it while it holds what that copy
//! gave it (its own last change, if any, is older): its current value from the copy's version on, its own history
//! before (absent where it has none). A key's first change after a copy records, at the copy's version, the value the
//! copy gave it, as the one history rule records a key's first change at version 0; a change in the copy's own version
//! records none, since the copy's value never stood at a version. A key's last change for the verdict is the later of
//! its own and the latest copy above it, and an edit declared against a base before such a copy is a conflict: the
//! bytes it was declared against were replaced whole, and no delta records the replacement.
//!
//! **A replace** (A-626, the `REPLACE` flag: Subversion's replace with history, a path its own commit deletes and adds
//! again with a copy source, §4.17.4 item 3.5) takes the destination's tree away before the copy lands. Its verdict
//! judges the removal as an unlink judges one: refused when the tree changed after the increment's base (`DeleteModify`,
//! a delete against a change) or holds a hard link or another link's target (`TypeChanged`: Subversion has none). Its
//! commit removes the tree key by key, each key's history recording its removal at the copy's version (the readers'
//! rule below counts such a removal as before the copy), so every holder answers the tree at the versions before; that
//! costs O(the replaced tree), as removing it key by key costs, and is charged before the verdict
//! (`Green::replace_history_reservation`, `Green::replace_changes_reservation`).
//!
//! **Operations on the copied tree in the same increment** (a commit that copies a directory and edits beneath it) are
//! judged against the copied tree, the source's tree at the head, and land after the copy: nothing intervenes in a tree
//! that was absent, so they are checked only for fitting what the copy brings. A file edited there is recorded as a
//! rename's edited destination is (A-547: its source's bytes with the edits spliced in), so replay reads the source as
//! the verdict did. A second copy into a copied tree (A-626) is judged against what the outer copy brings there, so it
//! needs the replace where that tree holds anything; Subversion's move inside a copied tree, a copy and a removal, lands
//! with it. Every source is held before any copy of the version lands, since FSFS copies from a revision and never from
//! the transaction, and the copies land in their destinations' order, an outer one before those inside its tree. A
//! rename or a hard link with an end in a copied tree is refused, typed: Subversion has neither.
//!
//! **The identity (A-202), relative to its copies.** The set hash adds one entry for a copy, naming its destination, its
//! source and the source's version, and never the entries the copy brought: those stand in the copy's entry, so a copy
//! costs the identity O(1) whatever the tree holds. Every key's entry moves as it always did, out at its value before a
//! change and in after it, and a key's first change after a copy also takes out the entry the copy gave it. So the sum
//! is the state's entries plus, for each copy, its own entry less the entries of the tree it copied: a function of the
//! state and its copies, recomputed by every holder that applies the same versions, and changed by any key that differs.
//! A green made at another's version starts from the other's sum at that version, which holds its copies' entries. A
//! replace's removals move the sum as removals always do; a key a version's copy brought and the same version's inner
//! replace took away gives back the outer copy's entry for it, which its fold reads from the copies above it.
//!
//! **Cost.** A copy's verdict reads the change index since the increment's base and since the source's version
//! (O(the entries since), as a branch's does) and makes descents in each map; its commit is A-542 part 1's copies,
//! O(the two paths' lengths) whatever the tree holds; it walks no map (`Green::walked` does not move). A reader pays one
//! lookup a level of the path's depth in the records, and nothing on a green that never copied.

use std::collections::{BTreeMap, BTreeSet};

use super::{
  ConflictWindow, ContentSource, Effect, Green, IDENTITY_TAG_HARDLINK, IDENTITY_TAG_SYMLINK,
  IdentityKey, Increment, Resolved, SPECIAL_NODE_BYTES, apply, change_bytes, copy_entry, dir_entry,
  file_entry, insert_whole, link_entry, mode_entry, record, special_entry, union_keys, xattr_entry,
};
use crate::dims::{Block, Held, HeldAttributes};
use crate::identity::Entry;
use crate::ops_doc::{Op, OpKind};
use crate::special::SpecialNode;
use crate::verdict::MergeConflictClass;

/// One copy a green committed, kept at its destination (module docs): the version it was made at, its source, and the
/// version of the source it copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Appearance {
  /// The version the copy was made at.
  pub(super) version: u64,
  /// The source's path.
  pub(super) source: String,
  /// The version of the source it copied.
  pub(super) source_version: u64,
}

/// A copy one increment declares (A-586, A-626): its source, the version of the source it copies, and whether it
/// replaces what its destination holds ([`crate::ops_doc::REPLACE`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CopySpec {
  /// The source's path.
  pub(super) source: String,
  /// The version of the source it copies.
  pub(super) version: u64,
  /// Whether it replaces what its destination holds.
  pub(super) replace: bool,
}

/// Every map's hold on a copy's source tree (A-626): taken before the version changes any map, so every copy of one
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

/// Derived: the bytes one copy's record costs, charged with the change index (A-586): its two paths' bytes and the
/// record's own size (the source's string header and the two versions), as `size_of` measures it on the target.
pub(super) fn appearance_bytes(dst: &str, src: &str) -> usize {
  dst
    .len()
    .saturating_add(src.len())
    .saturating_add(size_of::<Appearance>())
}

/// `path` and each of its ancestors, nearest first: the prefixes a copy above it could have been made at. The root is
/// none of them: no copy is made onto it.
pub(super) fn ancestors(path: &str) -> impl Iterator<Item = &str> {
  let mut next = (!path.is_empty()).then_some(path);
  std::iter::from_fn(move || {
    let current = next?;
    next = current
      .rfind('/')
      .and_then(|at| current.get(..at))
      .filter(|prefix| !prefix.is_empty());
    Some(current)
  })
}

/// Where `key` lies in `root`'s tree: the part of it below `root` (`""` for `root` itself), or `None` outside it. Every
/// key lies in the root's tree (the empty path's), whole.
pub(super) fn relative<'a>(key: &'a str, root: &str) -> Option<&'a str> {
  if root.is_empty() {
    return Some(key);
  }
  let rest = key.strip_prefix(root)?;
  if rest.is_empty() {
    Some(rest)
  } else {
    rest.strip_prefix('/')
  }
}

/// Whether `key` lies in `root`'s tree ([`relative`]).
pub(super) fn in_tree(key: &str, root: &str) -> bool {
  relative(key, root).is_some()
}

/// The path at `rel` in `root`'s tree: `root` itself for an empty `rel`, `rel` itself for the root.
pub(super) fn joined(root: &str, rel: &str) -> String {
  match (root.is_empty(), rel.is_empty()) {
    (_, true) => root.to_owned(),
    (true, false) => rel.to_owned(),
    (false, false) => format!("{root}/{rel}"),
  }
}

/// The prefix every path beneath `path` starts with: `path` and the separator, or nothing for the root.
fn beneath_prefix(path: &str) -> String {
  if path.is_empty() {
    String::new()
  } else {
    format!("{path}/")
  }
}

/// The innermost of this increment's copies whose tree holds `path` (A-626: copies nest, an outer one's tree holding
/// another's destination): the one with the longest destination, and its destination.
fn innermost<'a>(
  copies: &'a BTreeMap<String, CopySpec>,
  path: &str,
) -> Option<(&'a str, &'a CopySpec)> {
  copies
    .iter()
    .filter(|(dst, _)| in_tree(path, dst))
    .max_by_key(|(dst, _)| dst.len())
    .map(|(dst, spec)| (dst.as_str(), spec))
}

/// The innermost of this increment's other copies whose tree holds `dst`, a copy's destination: the copy it lands in.
fn outer_of<'a>(
  copies: &'a BTreeMap<String, CopySpec>,
  dst: &str,
) -> Option<(&'a str, &'a CopySpec)> {
  copies
    .iter()
    .filter(|(outer, _)| outer.as_str() != dst && in_tree(dst, outer))
    .max_by_key(|(outer, _)| outer.len())
    .map(|(outer, spec)| (outer.as_str(), spec))
}

/// The path in its copy's source that `path`, in this increment's copies' trees, stands for: the innermost copy's
/// source's own path for its destination, the same place beneath it otherwise. `path` itself outside every copy (never,
/// for the paths the split hands over).
fn source_of(copies: &BTreeMap<String, CopySpec>, path: &str) -> String {
  innermost(copies, path).map_or_else(
    || path.to_owned(),
    |(dst, spec)| joined(&spec.source, relative(path, dst).unwrap_or_default()),
  )
}

/// Takes out of `map` the entries whose path `inside` holds.
fn split_map<V>(
  map: &mut BTreeMap<String, V>,
  inside: &impl Fn(&str) -> bool,
) -> BTreeMap<String, V> {
  let (taken, kept) = std::mem::take(map)
    .into_iter()
    .partition(|(path, _)| inside(path.as_str()));
  *map = kept;
  taken
}

/// Takes out of `set` the paths `inside` holds.
fn split_set(set: &mut BTreeSet<String>, inside: &impl Fn(&str) -> bool) -> BTreeSet<String> {
  let (taken, kept) = std::mem::take(set)
    .into_iter()
    .partition(|path| inside(path.as_str()));
  *set = kept;
  taken
}

/// Takes out of `list` the entries whose path (`path_of`) `inside` holds.
fn split_list<T>(
  list: &mut Vec<T>,
  path_of: impl Fn(&T) -> &str,
  inside: &impl Fn(&str) -> bool,
) -> Vec<T> {
  let (taken, kept) = std::mem::take(list)
    .into_iter()
    .partition(|item| inside(path_of(item)));
  *list = kept;
  taken
}

impl Resolved {
  /// Hands every operation on a path in one of this increment's copies' trees to [`Resolved::copied`], judged against
  /// the copied tree (module docs), and marks invalid what this piece does not judge there: a rename or a hard link with
  /// either end in a copied tree (Subversion moves by a copy and a removal, which land; A-626).
  pub(super) fn split_copies(&mut self) {
    if self.copies.is_empty() {
      return;
    }
    let destinations: Vec<String> = self.copies.keys().cloned().collect();
    let inside = |path: &str| destinations.iter().any(|dst| in_tree(path, dst));
    for (dst, src) in std::mem::take(&mut self.renames) {
      if inside(dst.as_str()) || inside(src.as_str()) {
        self.invalid.insert(dst);
      } else {
        self.renames.insert(dst, src);
      }
    }
    for (path, target) in std::mem::take(&mut self.hardlinks) {
      if inside(path.as_str()) || inside(target.as_str()) {
        self.invalid.insert(path);
      } else {
        self.hardlinks.insert(path, target);
      }
    }
    self.copied = Some(Box::new(self.take_inside(&inside)));
  }

  /// The operations on paths `inside` holds, taken out of this one.
  fn take_inside(&mut self, inside: &impl Fn(&str) -> bool) -> Resolved {
    Resolved {
      content: split_map(&mut self.content, inside),
      creates: split_set(&mut self.creates, inside),
      specials: split_map(&mut self.specials, inside),
      unlinks: split_set(&mut self.unlinks, inside),
      mkdirs: split_set(&mut self.mkdirs, inside),
      rmdirs: split_set(&mut self.rmdirs, inside),
      modes: split_map(&mut self.modes, inside),
      symlinks: split_map(&mut self.symlinks, inside),
      xattr_sets: split_list(&mut self.xattr_sets, |(path, _, _)| path.as_str(), inside),
      xattr_removes: split_list(&mut self.xattr_removes, |(path, _)| path.as_str(), inside),
      ..Resolved::default()
    }
  }
}

impl Green {
  /// The latest copy whose tree holds `path` (its destination is `path` or an ancestor), with its destination: one
  /// lookup a level of the path's depth, and none on a green that never copied. Of copies one version made, the nearest
  /// is the latest: copies nest in their destinations' order, an inner one landing in an outer one's tree (A-626).
  pub(super) fn copy_above(&self, path: &str) -> Option<(&str, &Appearance)> {
    if self.copies.is_empty() {
      return None;
    }
    let mut latest: Option<(&str, &Appearance)> = None;
    for prefix in ancestors(path) {
      if let Some((dst, records)) = self.copies.get_key_value(prefix)
        && let Some(record) = records.last()
        && latest.is_none_or(|(_, best)| record.version > best.version)
      {
        latest = Some((dst.as_str(), record));
      }
    }
    latest
  }

  /// Every copy whose tree holds `path` made after `since`, with its destination.
  pub(super) fn copies_above_after(&self, path: &str, since: u64) -> Vec<(&str, &Appearance)> {
    if self.copies.is_empty() {
      return Vec::new();
    }
    ancestors(path)
      .filter_map(|prefix| self.copies.get_key_value(prefix))
      .flat_map(|(dst, records)| {
        records
          .iter()
          .filter(move |record| record.version > since)
          .map(move |record| (dst.as_str(), record))
      })
      .collect()
  }

  /// The version since which `path` has held what a copy gave it (module docs): the latest copy above it, when the
  /// path has no history, or its own last recorded change (`own_last`: its version, and whether it removed the key) is
  /// older, or is a removal at the copy's own version, which a replace makes before the copy lands (A-626).
  pub(super) fn covered_since(&self, path: &str, own_last: Option<(u64, bool)>) -> Option<u64> {
    let (_, record) = self.copy_above(path)?;
    own_last
      .is_none_or(|(last, removed)| last < record.version || (last == record.version && removed))
      .then_some(record.version)
  }

  /// The version a key's held value dates from, recorded before its change (`super::seed`): the copy's that gave it,
  /// while it holds that copy's value; 0 for a key with no history (A-534's one history rule); none when its history
  /// already holds it.
  pub(super) fn seed_since(&self, path: &str, own_last: Option<(u64, bool)>) -> Option<u64> {
    self
      .covered_since(path, own_last)
      .or_else(|| own_last.is_none().then_some(0))
  }

  /// A key's last change for the verdict: the later of its own (`own`) and the latest copy above `path`.
  pub(super) fn changed_last(&self, own: Option<&u64>, path: &str) -> u64 {
    let own = own.copied().unwrap_or(0);
    self
      .copy_above(path)
      .map_or(own, |(_, record)| own.max(record.version))
  }

  /// Whether the latest copy above `path` was made at `version`: what the path holds then is that copy's, edited by its
  /// own increment at most.
  pub(super) fn copied_in(&self, path: &str, version: u64) -> bool {
    self
      .copy_above(path)
      .is_some_and(|(_, record)| record.version == version)
  }

  /// Whether a copy above `path` replaced its tree after `base` (module docs): an edit declared against `base` meets
  /// bytes the copy brought whole.
  pub(super) fn replaced_after(&self, path: &str, base: u64) -> bool {
    self
      .copy_above(path)
      .is_some_and(|(_, record)| record.version > base)
  }

  /// Whether any map holds a key in `path`'s tree (any key, for the root). Descents, no walk.
  pub(super) fn tree_held(&self, path: &str) -> bool {
    self.content.holds_tree(path)
      || self.specials.holds_tree(path)
      || self.dirs.holds_tree(path)
      || self.modes.holds_tree(path)
      || self.symlinks.holds_tree(path)
      || self.hardlinks.holds_tree(path)
      || self.xattrs.holds_tree(path)
  }

  /// Whether anything in `path`'s tree changed after `version`: a copy above it made since, or an entry of the change
  /// index in `(version, head]` at the path or beneath it (any entry, for the root). O(the entries since `version`); the
  /// index keeps each path's last change below its fold floor, so a span reaching below it is answered as well.
  pub(super) fn tree_changed_after(&self, path: &str, version: u64) -> bool {
    if self.replaced_after(path, version) {
      return true;
    }
    self
      .changes
      .range(version.saturating_add(1)..)
      .flat_map(|(_, entries)| entries)
      .any(|entry| in_tree(&entry.path, path))
  }

  /// The verdict of each copy the increment declares (module docs), its effect or its conflict: a copy into another's
  /// tree is judged against that tree (A-626), every other against the head.
  pub(super) fn decide_copies(
    &self,
    inc: &Increment,
    r: &Resolved,
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    for (dst, spec) in &r.copies {
      let verdict = match outer_of(&r.copies, dst) {
        None => self.decide_copy(inc.base, dst, spec),
        Some(outer) => self.decide_nested_copy(dst, spec, outer),
      };
      match verdict {
        Ok(effect) => effects.push(effect),
        Err(window) => windows.push(window),
      }
    }
  }

  /// One copy's verdict against the head (module docs): the conflict names the destination, by kind.
  fn decide_copy(&self, base: u64, dst: &str, spec: &CopySpec) -> Result<Effect, ConflictWindow> {
    if dst.is_empty() {
      return Err(Green::window(dst, MergeConflictClass::TypeChanged));
    }
    if let Some(class) = self.destination_refused(base, dst, spec.replace) {
      return Err(Green::window(dst, class));
    }
    self.copy_of_source(dst, spec)
  }

  /// Why the head's `dst` takes no copy, if it takes none: a tree there and no replace (`CreateCreate`, an add of a
  /// present path), a replaced tree changed after the base (`DeleteModify`, a delete against a change) or holding a hard
  /// link or a link's target (`TypeChanged`: Subversion has none, A-626), or an absent tree changed after the base
  /// (`CreateCreate`).
  fn destination_refused(&self, base: u64, dst: &str, replace: bool) -> Option<MergeConflictClass> {
    if !self.tree_held(dst) {
      return self
        .tree_changed_after(dst, base)
        .then_some(MergeConflictClass::CreateCreate);
    }
    if !replace {
      return Some(MergeConflictClass::CreateCreate);
    }
    if self.tree_changed_after(dst, base) {
      return Some(MergeConflictClass::DeleteModify);
    }
    (self.hardlinks.holds_tree(dst) || self.links_to.holds_tree(dst))
      .then_some(MergeConflictClass::TypeChanged)
  }

  /// A copy into another copy's tree in the same increment (A-626, a second copy into a copied tree; Subversion's move
  /// within one is a copy and a removal): its destination is judged against the outer copy's source, which the outer
  /// copy's verdict holds unchanged since its version, so a tree there takes the copy only as a replace.
  fn decide_nested_copy(
    &self,
    dst: &str,
    spec: &CopySpec,
    (outer, outer_spec): (&str, &CopySpec),
  ) -> Result<Effect, ConflictWindow> {
    let there = joined(&outer_spec.source, relative(dst, outer).unwrap_or_default());
    if self.tree_held(&there) && !spec.replace {
      return Err(Green::window(dst, MergeConflictClass::CreateCreate));
    }
    self.copy_of_source(dst, spec)
  }

  /// The source's half of a copy's verdict (module docs), and the copy's effect: refused when the source version is past
  /// the head or nothing is there, when the source holds a hard link (`TypeChanged`), and when its tree changed after
  /// that version (`RenameRename`).
  fn copy_of_source(&self, dst: &str, spec: &CopySpec) -> Result<Effect, ConflictWindow> {
    let refused = |class| Err(Green::window(dst, class));
    if spec.version > self.head() {
      return refused(MergeConflictClass::TypeChanged);
    }
    if self.tree_changed_after(&spec.source, spec.version) {
      return refused(MergeConflictClass::RenameRename);
    }
    if !self.tree_held(&spec.source) || self.hardlinks.holds_tree(&spec.source) {
      return refused(MergeConflictClass::TypeChanged);
    }
    Ok(Effect::CopyTree(
      dst.to_owned(),
      spec.source.clone(),
      spec.version,
      spec.replace,
    ))
  }

  /// The operations on this increment's copied trees (module docs), judged against the copied tree: the source's at the
  /// head.
  pub(super) fn decide_copied(
    &self,
    inc: &Increment,
    r: &Resolved,
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    let Some(copied) = r.copied.as_deref() else {
      return;
    };
    let from = |path: &str| source_of(&r.copies, path);
    self.decide_copied_content(inc, copied, &from, effects, windows);
    self.decide_copied_names(copied, &from, effects, windows);
    self.decide_copied_metadata(copied, &from, effects, windows);
  }

  /// [`Green::decide_copied`] of creations and edits of files.
  fn decide_copied_content(
    &self,
    inc: &Increment,
    copied: &Resolved,
    from: &impl Fn(&str) -> String,
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    for path in &copied.creates {
      let ops = copied.content.get(path).map(Vec::as_slice).unwrap_or(&[]);
      record(
        effects,
        windows,
        self.copied_create(path, &from(path.as_str()), ops, &inc.post_state),
      );
    }
    for (path, ops) in &copied.content {
      if !copied.creates.contains(path) {
        record(
          effects,
          windows,
          self.copied_edit(path, &from(path.as_str()), ops, &inc.post_state),
        );
      }
    }
    for (path, node) in &copied.specials {
      let refused = self.holds_node(&from(path.as_str()));
      record(
        effects,
        windows,
        Green::unless(refused, path, Effect::Special(path.clone(), *node)),
      );
    }
  }

  /// [`Green::decide_copied`] of removals and directories.
  fn decide_copied_names(
    &self,
    copied: &Resolved,
    from: &impl Fn(&str) -> String,
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    for path in &copied.unlinks {
      record(
        effects,
        windows,
        Ok(self.copied_unlink(path, &from(path.as_str()))),
      );
    }
    for path in &copied.mkdirs {
      record(
        effects,
        windows,
        self.copied_mkdir(path, &from(path.as_str())),
      );
    }
    let cleared: BTreeSet<String> = copied
      .cleared()
      .iter()
      .map(|path| from(path.as_str()))
      .collect();
    for path in &copied.rmdirs {
      record(
        effects,
        windows,
        self.copied_rmdir(path, &from(path.as_str()), &cleared),
      );
    }
  }

  /// [`Green::decide_copied`] of modes, symbolic links and attributes.
  fn decide_copied_metadata(
    &self,
    copied: &Resolved,
    from: &impl Fn(&str) -> String,
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    let absent =
      |path: &String| !self.node_stands(&from(path.as_str())) && !copied.establishes(path);
    for (path, mode) in &copied.modes {
      let effect = Effect::Mode(path.clone(), *mode);
      record(
        effects,
        windows,
        Green::unless_absent(absent(path), path, effect),
      );
    }
    for (path, target) in &copied.symlinks {
      let refused =
        self.holds_node(&from(path.as_str())) && !self.symlinks.contains_key(from(path.as_str()));
      let effect = Effect::Symlink(path.clone(), target.clone());
      record(effects, windows, Green::unless(refused, path, effect));
    }
    for (path, name, value) in &copied.xattr_sets {
      let effect = Effect::SetXattr(path.clone(), name.clone(), value.clone());
      record(
        effects,
        windows,
        Green::unless_absent(absent(path), path, effect),
      );
    }
    for (path, name) in &copied.xattr_removes {
      let held = self.xattrs.contains_key(&from(path.as_str()), name);
      let effect = held.then(|| Effect::RemoveXattr(path.clone(), name.clone()));
      record(effects, windows, Ok(effect));
    }
  }

  /// `effect`, or a type conflict on `path` when `refused`.
  fn unless(refused: bool, path: &str, effect: Effect) -> Result<Option<Effect>, ConflictWindow> {
    if refused {
      Err(Green::window(path, MergeConflictClass::TypeChanged))
    } else {
      Ok(Some(effect))
    }
  }

  /// `effect`, or a delete/modify conflict on `path` when what it changes is `absent`.
  fn unless_absent(
    absent: bool,
    path: &str,
    effect: Effect,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if absent {
      Err(Green::window(path, MergeConflictClass::DeleteModify))
    } else {
      Ok(Some(effect))
    }
  }

  /// Whether a file, directory, symbolic link, hard link or IPC node stands at `path`.
  fn holds_node(&self, path: &str) -> bool {
    self.node_stands(path) || self.symlinks.contains_key(path) || self.hardlinks.contains_key(path)
  }

  /// Whether a file, directory or IPC node stands at `path`: what a mode or an attribute can be set on.
  fn node_stands(&self, path: &str) -> bool {
    self.content.contains_key(path)
      || self.dirs.contains_key(path)
      || self.specials.contains_key(path)
  }

  /// A file created at `path` in a copied tree, where the copy brings nothing (`from`, its place in the source).
  fn copied_create(
    &self,
    path: &str,
    from: &str,
    ops: &[Op],
    post_state: &[u8],
  ) -> Result<Option<Effect>, ConflictWindow> {
    if self.holds_node(from) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    let bytes = apply(&[], ops, post_state);
    let whole = vec![insert_whole(bytes.len() as u64)];
    Ok(Some(Effect::SetContent(
      path.to_owned(),
      bytes,
      whole,
      ContentSource::Created(ops.to_vec()),
    )))
  }

  /// A file the copy brings to `path` (from `from`), edited: the source's bytes with the edits spliced in, recorded as a
  /// rename's edited destination is (A-547), so replay splices them into the source's bytes as the verdict did.
  fn copied_edit(
    &self,
    path: &str,
    from: &str,
    ops: &[Op],
    post_state: &[u8],
  ) -> Result<Option<Effect>, ConflictWindow> {
    let Some(source) = self.content.get(from) else {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    };
    let bytes = apply(source, ops, post_state);
    let whole = vec![insert_whole(bytes.len() as u64)];
    Ok(Some(Effect::SetContent(
      path.to_owned(),
      bytes,
      whole,
      ContentSource::Copied(from.to_owned(), ops.to_vec()),
    )))
  }

  /// A name the copy brings to `path` removed: whatever its source (`from`) is, a file, a symbolic link or an IPC node;
  /// nothing for a name the copy does not bring, as an unlink of an absent name is nothing (`merge_unlink`).
  fn copied_unlink(&self, path: &str, from: &str) -> Option<Effect> {
    if self.content.contains_key(from) {
      Some(Effect::RemoveContent(path.to_owned()))
    } else if self.symlinks.contains_key(from) {
      Some(Effect::RemoveSymlink(path.to_owned()))
    } else if self.specials.contains_key(from) {
      Some(Effect::RemoveSpecial(path.to_owned()))
    } else {
      None
    }
  }

  /// A directory made at `path` in a copied tree: nothing where the copy brings one, a type conflict where it brings
  /// another kind.
  fn copied_mkdir(&self, path: &str, from: &str) -> Result<Option<Effect>, ConflictWindow> {
    if self.dirs.contains_key(from) {
      return Ok(None);
    }
    if self.holds_node(from) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    Ok(Some(Effect::MakeDir(path.to_owned())))
  }

  /// A directory the copy brings to `path` removed, once the increment's own removals (`cleared`, in the source's
  /// paths) empty it, as `merge_rmdir` judges one.
  fn copied_rmdir(
    &self,
    path: &str,
    from: &str,
    cleared: &BTreeSet<String>,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if !self.dirs.contains_key(from) {
      if self.holds_node(from) {
        return Err(Green::window(path, MergeConflictClass::TypeChanged));
      }
      return Ok(None);
    }
    // The directory's own key is no child of it (the root's tree holds the root's own key).
    let mut gone = cleared.clone();
    gone.insert(from.to_owned());
    if self.has_live_child(&beneath_prefix(from), &gone) {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    Ok(Some(Effect::RemoveDir(path.to_owned())))
  }

  /// The paths whose content ops in this increment's copied trees do not fit what they edit (`super::fits_base`): a
  /// created path edits nothing, an edited one its source's bytes at the head.
  pub(super) fn unfit_copied(&self, inc: &Increment, r: &Resolved) -> Vec<String> {
    let Some(copied) = r.copied.as_deref() else {
      return Vec::new();
    };
    let post_len = u64::try_from(inc.post_state.len()).unwrap_or(u64::MAX);
    copied
      .content
      .iter()
      .filter(|(path, ops)| {
        let base_len = if copied.creates.contains(*path) {
          0
        } else {
          let from = source_of(&r.copies, path);
          self
            .content
            .get(&from)
            .map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
        };
        !super::fits_base(ops, base_len, post_len)
      })
      .map(|(path, _)| path.clone())
      .collect()
  }

  /// Applies the copies among a version's accepted effects, in their destinations' order, an outer copy before one
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
      self
        .identity_dirty
        .insert(IdentityKey::Special(path.clone()));
      self.remove_special(path.clone(), version);
    }
    let mut staged = super::Staged::default();
    let removals: Vec<Effect> = tree
      .dirs
      .iter()
      .map(|path| Effect::RemoveDir(path.clone()))
      .chain(
        tree
          .symlinks
          .iter()
          .map(|path| Effect::RemoveSymlink(path.clone())),
      )
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

  /// The destinations of the replaces `inc` declares (A-626), each with what another copy `inc` declares brings to it
  /// first (that copy's source at the destination's place in its tree): a replace takes away the head's tree at its
  /// destination and whatever those copies bring there.
  fn replaces_declared(inc: &Increment) -> Vec<(&str, Vec<String>)> {
    let name = |index: u64| {
      u16::try_from(index)
        .ok()
        .and_then(|index| inc.doc.paths.path(index))
    };
    let copies: Vec<(&str, &str)> = inc
      .doc
      .ops
      .iter()
      .filter(|op| op.kind == OpKind::Copy)
      .filter_map(|op| Some((name(u64::from(op.path))?, name(op.src)?)))
      .collect();
    inc
      .doc
      .ops
      .iter()
      .filter(|op| op.replaces())
      .filter_map(|op| name(u64::from(op.path)))
      .map(|dst| {
        let brought = copies
          .iter()
          .filter_map(|(outer, source)| {
            relative(dst, outer)
              .filter(|rel| !rel.is_empty())
              .map(|rel| joined(source, rel))
          })
          .collect();
        (dst, brought)
      })
      .collect()
  }

  /// [`Green::history_reservation`]'s share for the replaces `inc` declares (A-626): each file the head's tree at a
  /// replaced destination holds keeps its latest value, each IPC node its node. What a copy of the same version brought
  /// there keeps nothing: its values never stood at a version. An upper bound: a replace inside another's tree counts
  /// twice.
  pub(super) fn replace_history_reservation(&self, inc: &Increment) -> usize {
    Green::replaces_declared(inc)
      .into_iter()
      .map(|(dst, _)| {
        let tree = self.tree_keys(dst);
        let nodes = tree.specials.len().saturating_mul(SPECIAL_NODE_BYTES);
        tree
          .files
          .iter()
          .map(|path| self.latest_bytes(path))
          .fold(nodes, usize::saturating_add)
      })
      .fold(0, usize::saturating_add)
  }

  /// [`Green::changes_reservation`]'s share for the replaces `inc` declares (A-626): each path a replace takes away,
  /// named at its destination, at the new version and as its last change.
  pub(super) fn replace_changes_reservation(&self, inc: &Increment) -> usize {
    let mut total = 0usize;
    for (dst, brought) in Green::replaces_declared(inc) {
      let roots = std::iter::once(dst.to_owned()).chain(brought);
      for root in roots {
        let named = |path: &String| {
          let at = joined(dst, relative(path, &root).unwrap_or_default());
          change_bytes(&at).saturating_mul(2)
        };
        let tree = self.tree_keys(&root).paths();
        total = tree.iter().map(named).fold(total, usize::saturating_add);
      }
    }
    total
  }

  /// The entries of the copies made at `dst` after `since`: what a copy puts into the set hash (module docs).
  pub(super) fn copy_entries_after(&self, dst: &str, since: u64) -> Vec<Entry> {
    self
      .copies
      .get(dst)
      .into_iter()
      .flatten()
      .filter(|record| record.version > since)
      .map(|record| copy_entry(dst, &record.source, record.source_version))
      .collect()
  }

  /// The entries copies made after `since` gave `key` (module docs): for each copy above its path made since, the key's
  /// entry with the value the copy's source held at the source's version, where it held one.
  pub(super) fn copied_entries(&self, key: &IdentityKey, since: u64) -> Vec<Entry> {
    let path = key.path();
    self
      .copies_above_after(path, since)
      .into_iter()
      .filter_map(|(dst, record)| {
        let rel = relative(path, dst)?;
        self.entry_at(key, &joined(&record.source, rel), record.source_version)
      })
      .collect()
  }

  /// `key`'s entry, its value read at `from` and `version` (`from` its own path, or its place in a copy's source).
  fn entry_at(&self, key: &IdentityKey, from: &str, version: u64) -> Option<Entry> {
    match key {
      IdentityKey::File(path) => self
        .content_ref_at(from, version)
        .map(|bytes| file_entry(path, &bytes)),
      IdentityKey::Special(path) => self
        .special_at(from, version)
        .map(|node| special_entry(path, &node)),
      IdentityKey::Dir(path) => self.dir_at(from, version).then(|| dir_entry(path)),
      IdentityKey::Mode(path) => self
        .mode_at(from, version)
        .map(|mode| mode_entry(path, mode)),
      IdentityKey::Symlink(path) => self
        .symlink_at(from, version)
        .map(|target| link_entry(IDENTITY_TAG_SYMLINK, path, &target)),
      IdentityKey::Hardlink(path) => self
        .hardlink_at(from, version)
        .map(|target| link_entry(IDENTITY_TAG_HARDLINK, path, &target)),
      IdentityKey::Xattr(path, name) => self
        .xattr_at(from, name, version)
        .map(|value| xattr_entry(path, name, &value)),
      IdentityKey::Copy(_) => None,
    }
  }

  /// Every entry the copy `record` at `dst` brought, named at the destination: its source's tree at the source's version,
  /// key by key. O(everything the green holds): what [`Green::head_identity_from_scratch`] takes out for each copy, which
  /// the running sum never adds (module docs). It reads each source at its version, so it holds while the histories
  /// reach back to every copy's source version that changed since.
  pub(super) fn copied_tree_entries(&self, dst: &str, record: &Appearance) -> Vec<Entry> {
    let (src, version) = (record.source.as_str(), record.source_version);
    self
      .every_key(src)
      .into_iter()
      .filter_map(|key| {
        let rel = relative(key.path(), src)?;
        self.entry_at(&key.renamed(&joined(dst, rel)), key.path(), version)
      })
      .collect()
  }

  /// Every key of every dimension the green holds or held in `root`'s tree, current or recorded.
  fn every_key(&self, root: &str) -> Vec<IdentityKey> {
    let inside = |path: &String| in_tree(path, root);
    let mut keys: Vec<IdentityKey> = Vec::new();
    let files = union_keys(self.content.keys(), self.content_history.keys().cloned());
    keys.extend(files.filter(inside).map(IdentityKey::File));
    let specials = union_keys(self.specials.keys(), self.special_history.keys().cloned());
    keys.extend(specials.filter(inside).map(IdentityKey::Special));
    let dirs = union_keys(self.dirs.keys(), self.dir_history.keys().cloned());
    keys.extend(dirs.filter(inside).map(IdentityKey::Dir));
    let modes = union_keys(self.modes.keys(), self.mode_history.keys().cloned());
    keys.extend(modes.filter(inside).map(IdentityKey::Mode));
    let symlinks = union_keys(self.symlinks.keys(), self.symlink_history.keys().cloned());
    keys.extend(symlinks.filter(inside).map(IdentityKey::Symlink));
    let hardlinks = union_keys(self.hardlinks.keys(), self.hardlink_history.keys().cloned());
    keys.extend(hardlinks.filter(inside).map(IdentityKey::Hardlink));
    let xattrs = union_keys(
      self.xattrs.iter().map(|(key, _)| key),
      self.xattr_history.keys().cloned(),
    );
    keys.extend(
      xattrs
        .filter(|(path, _)| inside(path))
        .map(|(path, name)| IdentityKey::Xattr(path, name)),
    );
    keys
  }
}

impl IdentityKey {
  /// The path the key is at (an attribute's, the path it is on; a copy's, its destination).
  pub(super) fn path(&self) -> &str {
    match self {
      IdentityKey::File(path)
      | IdentityKey::Special(path)
      | IdentityKey::Dir(path)
      | IdentityKey::Mode(path)
      | IdentityKey::Symlink(path)
      | IdentityKey::Hardlink(path)
      | IdentityKey::Xattr(path, _)
      | IdentityKey::Copy(path) => path,
    }
  }

  /// The same key at `path`.
  fn renamed(&self, path: &str) -> IdentityKey {
    let path = path.to_owned();
    match self {
      IdentityKey::File(_) => IdentityKey::File(path),
      IdentityKey::Special(_) => IdentityKey::Special(path),
      IdentityKey::Dir(_) => IdentityKey::Dir(path),
      IdentityKey::Mode(_) => IdentityKey::Mode(path),
      IdentityKey::Symlink(_) => IdentityKey::Symlink(path),
      IdentityKey::Hardlink(_) => IdentityKey::Hardlink(path),
      IdentityKey::Xattr(_, name) => IdentityKey::Xattr(path, name.clone()),
      IdentityKey::Copy(_) => IdentityKey::Copy(path),
    }
  }
}
