//! A path's tree copied by reference in the merge engine (A-586, A-626; A-542's second half; §4.17.4 item 3 and piece
//! 3; §4.16 "The state's maps"): Subversion's copy, a branch or a tag made from any path at a version, which a stock
//! client's commit declares as an addition with a copy source, or as a replacement with one.
//!
//! **What a copy is.** `Copy` (`OpKind` 16) names its destination, its source and the source's version (Subversion's
//! copy-from revision: FSFS copies only from a committed revision, `libsvn_fs_fs/tree.c` `copy_helper`). The verdict
//! judges it node-exclusively, an addition: refused when the destination or anything beneath it is present at the head
//! (unless the copy replaces it, below) or changed after the increment's base, when nothing stood in the source's tree
//! at its version, when a hard link in the source's tree names a node outside it that changed after the version (below),
//! and when the destination is the root (always present; an editor never adds it). Accepted, the head's maps copy the source's tree by reference (`crate::dims`, "A tree is copied by
//! reference"). The root is a source like any other: its tree is every key (a stock client copies `^/` into a child,
//! `libsvn_client/copy.c` refusing a copy into its own child only between working copies).
//!
//! **A hard link in the copied tree** (main's ruling of 2026-11-20: an agent's workspace holds hard links routinely, a
//! `git clone --local` linking its objects). The rule is a snapshot's: names in the copied tree that share a node keep
//! sharing one new node in the copy, so a copied link names its node's copy; a name whose node lies outside the copied
//! tree gets a node of its own in the copy, holding that node's bytes, mode or IPC node, its bytes named by the content
//! they hold, never copied by the outside node. The graft shares the map of links as it shares every map, so a copied
//! link still names its node in the source; the version's commit then derives each copied link's own effect
//! ([`Green::copy_link_effects`]: a link naming the copy's node, or the link removed and a node of its own made), from
//! the head before the copies land, as a node's moves are derived, so a replay derives the same and the chain entry
//! records none. The node outside is read from the head, so the verdict refuses a copy whose outside nodes changed after
//! its version ([`Green::outside_link_moved`]).
//!
//! **A copy from a version its source changed after** (A-626 part 2; FSFS copies from any committed revision, its
//! node-revisions kept whole) brings the tree as it stood then. The maps hold only the head, so the copy grafts the
//! head's tree and fixes it (`CopyFix`, in `crate::chain_entry`): the paths the change index names in the source's
//! tree since the version are the only keys that can differ, each set to its value then where that differs from the
//! head's; a copy made since inside the source is cut; one made since over it replaced the whole tree, so nothing of the
//! head's is grafted and its keys then (each named by its removal) are set whole. The verdict reads the fix through a
//! view of the source as the copy brings it (`SourceAt`): whether anything stood there is counted, never walked
//! (`count_tree`, the radix tree's node counts). The fix is set at the commit with no key recorded: the copy's entry in
//! the identity already stands for the source's tree at its version, and a key's first change after the copy records
//! the value the copy gave it. The chain entry records the fix whole, so a replay reads no history whatever its holder
//! folded or restored, and the room it needs is derived before the verdict (`Green::copy_entry_room`). A version whose
//! values are folded away is refused, typed (`CopyRefusal::VersionFolded`): what it held is gone.
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
//! rename or a hard link with an end in a copied tree is refused, typed (`CopyRefusal::LinkAcrossCopy`): Subversion has
//! neither.
//!
//! **Refused before any verdict, typed** (main's ruling of 2026-11-20; [`CopyRefusal`], `Green::copy_refusal`): a copy
//! this piece does not make is answered as its own case, never a conflict (no concurrent change caused it, no rebase
//! answers it), by a submit and a rebase alike (`Outcome::CopyRefused`, `Rebased::CopyRefused`): the root as destination,
//! a source version past the head and an empty source, which FSFS's commit refuses too; a version folded away, which
//! follows from what a green retains; a hard link naming an outside node changed since the version, a hard link in a
//! source changed since it, and a rename or link across a copied tree's edge, owed work. The verdict's own checks of
//! these stay as guards, never reached.
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
//! **What a copy brought is read from its graft** (A-626 part 3): its source held at the version before the copy, with
//! the copy's fix applied, the fix kept with its record (charged with the change index) until the fold floor reaches
//! the copy (`Green::brought_entry`). Every reader of it asks for a copy above the floor (the identity is brought past a
//! copy before the floor passes it; a branch or a checkpoint is made at the floor or above), and the version before such
//! a copy is never below the floor, nor a restored green's checkpoint. The source's own history at the source's version
//! may be: a green restored at a checkpoint after that version and before the copy holds none of it, which a replay of
//! the copy met (`docs/bugs/2026-11-19-a-copy-from-before-a-checkpoint-hashed-otherwise-on-its-replica.md`). The map's
//! current value would answer a key the copy gave and nothing changed since, and a key's own history at the copy's
//! version one changed after, but neither answers a key the copy's own version changed or an inner copy of that version
//! replaced, whose value stood at no version: only the graft holds it. Recording the entries a copy brought would cost
//! the copy O(its tree) in the record and the checkpoint, and was rejected (A-626 part 3's entry).
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
use crate::chain_entry::{Applied, CopyFix};
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
  /// The copy's fix (A-626 part 2), kept while the copy stands above the green's fold floor, charged with the change
  /// index: what the identity reads the copy's tree through, with its source held at the version before the copy
  /// ([`Green::brought_entry`], A-626 part 3). None for a copy whose source was unchanged since its version, whose
  /// graft brought it whole, and once the floor reaches the copy ([`Green::release_kept_fixes`]); a checkpoint never
  /// carries one.
  pub(super) fix: Option<Box<CopyFix>>,
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

/// Each copy's fix in one increment (A-626 part 2), by destination, computed once a verdict ([`Green::copy_fixes`]):
/// none for a source unchanged since its version, the class that refuses one whose version's values are folded away or
/// held a hard link.
pub(super) type CopyFixes = BTreeMap<String, Result<Option<CopyFix>, CopyRefusal>>;

/// One copy as [`Green::commit_copies`] applies it: its destination and source, the source's version and whether it
/// replaces, and its fix.
type Committed<'a> = ((&'a str, &'a str), (u64, bool), Option<&'a CopyFix>);

/// Whether a copy with `fix` grafts the head's keys at `rel` (relative to its source): not where it starts empty, nor
/// in a cut.
fn grafted_in(fix: &CopyFix, rel: &str) -> bool {
  !fix.start_empty && !fix.cuts.iter().any(|cut| in_tree(rel, cut))
}

/// The entry `fix` sets `key` (named at a copy's destination, at `rel` in its tree) to, where the fix names it: the
/// key's entry with the fix's value, or none where the fix takes it away (A-626 part 3, [`Green::brought_entry`]).
/// `None` where the fix does not name it; never one for a hard link, which no copied tree holds.
fn fixed_entry(key: &IdentityKey, rel: &str, fix: &CopyFix) -> Option<Option<Entry>> {
  match key {
    IdentityKey::File(path) => fix
      .files
      .get(rel)
      .map(|bytes| bytes.as_deref().map(|bytes| file_entry(path, bytes))),
    IdentityKey::Special(path) => fix
      .specials
      .get(rel)
      .map(|node| node.as_ref().map(|node| special_entry(path, node))),
    IdentityKey::Dir(path) => fix
      .dirs
      .get(rel)
      .map(|stood| stood.then(|| dir_entry(path))),
    IdentityKey::Mode(path) => fix
      .modes
      .get(rel)
      .map(|mode| mode.map(|mode| mode_entry(path, mode))),
    IdentityKey::Symlink(path) => fix.symlinks.get(rel).map(|target| {
      target
        .as_deref()
        .map(|target| link_entry(IDENTITY_TAG_SYMLINK, path, target))
    }),
    IdentityKey::Xattr(path, name) => fix
      .xattrs
      .get(&(rel.to_owned(), name.clone()))
      .map(|value| value.as_deref().map(|value| xattr_entry(path, name, value))),
    IdentityKey::Hardlink(_) | IdentityKey::Copy(_) => None,
  }
}

/// Every key `fix` names, named at the destination `dst` (A-626 part 3, [`Green::copied_tree_entries`]).
fn fixed_keys(dst: &str, fix: &CopyFix) -> Vec<IdentityKey> {
  let at = |rel: &String| joined(dst, rel);
  let mut keys: Vec<IdentityKey> = Vec::new();
  keys.extend(fix.files.keys().map(|rel| IdentityKey::File(at(rel))));
  keys.extend(fix.specials.keys().map(|rel| IdentityKey::Special(at(rel))));
  keys.extend(fix.dirs.keys().map(|rel| IdentityKey::Dir(at(rel))));
  keys.extend(fix.modes.keys().map(|rel| IdentityKey::Mode(at(rel))));
  keys.extend(fix.symlinks.keys().map(|rel| IdentityKey::Symlink(at(rel))));
  keys.extend(
    fix
      .xattrs
      .keys()
      .map(|(rel, name)| IdentityKey::Xattr(at(rel), name.clone())),
  );
  keys
}

/// Counts one key of a fix against the head's graft ([`SourceAt::holds_tree`]): one gained where the fix sets a key the
/// graft does not bring, one lost where it takes away one the graft brings.
fn tally(counts: &mut (usize, usize), then: bool, brought: bool) {
  if then && !brought {
    counts.0 = counts.0.saturating_add(1);
  } else if !then && brought {
    counts.1 = counts.1.saturating_add(1);
  }
}

/// A copy's source as the copy brings it (A-626 part 2): the head's tree, or, for a source changed after the copied
/// version, the head's tree less its fix's cuts with its fix's values set, which is the source's tree at the version.
/// What a verdict judges a copied tree against, and what replay reads an edited file's bytes from.
#[derive(Clone, Copy)]
pub(super) struct SourceAt<'a> {
  green: &'a Green,
  source: &'a str,
  fix: Option<&'a CopyFix>,
}

impl<'a> SourceAt<'a> {
  /// Where `path` lies in the source's tree, relative to it, and whether the head's keys there are grafted; `None`
  /// outside the tree.
  fn place<'p>(&self, path: &'p str) -> Option<(&'p str, bool)> {
    let rel = relative(path, self.source)?;
    Some((rel, self.fix.is_none_or(|fix| grafted_in(fix, rel))))
  }

  /// The file's bytes at `path`.
  pub(super) fn content(&self, path: &str) -> Option<&'a [u8]> {
    let Some(fix) = self.fix else {
      return self.green.content.get(path);
    };
    let (rel, grafted) = self.place(path)?;
    match fix.files.get(rel) {
      Some(bytes) => bytes.as_deref(),
      None => self.green.content.get(path).filter(|_| grafted),
    }
  }

  /// Whether a directory stands at `path`.
  fn dir(&self, path: &str) -> bool {
    let Some(fix) = self.fix else {
      return self.green.dirs.contains_key(path);
    };
    let Some((rel, grafted)) = self.place(path) else {
      return false;
    };
    match fix.dirs.get(rel) {
      Some(stood) => *stood,
      None => grafted && self.green.dirs.contains_key(path),
    }
  }

  /// The IPC node at `path`.
  fn special(&self, path: &str) -> Option<SpecialNode> {
    let Some(fix) = self.fix else {
      return self.green.specials.get(path).copied();
    };
    let (rel, grafted) = self.place(path)?;
    match fix.specials.get(rel) {
      Some(node) => *node,
      None => self.green.specials.get(path).copied().filter(|_| grafted),
    }
  }

  /// The symbolic link's target at `path`.
  fn symlink(&self, path: &str) -> Option<&'a str> {
    let head = self.green.symlinks.get(path).map(String::as_str);
    let Some(fix) = self.fix else {
      return head;
    };
    let (rel, grafted) = self.place(path)?;
    match fix.symlinks.get(rel) {
      Some(target) => target.as_deref(),
      None => head.filter(|_| grafted),
    }
  }

  /// Whether a hard link stands at `path` (a fix never sets one: its version held none).
  fn hardlink(&self, path: &str) -> bool {
    self.place(path).is_some_and(|(_, grafted)| grafted) && self.green.hardlinks.contains_key(path)
  }

  /// The attribute `name`'s value at `path`.
  fn xattr(&self, path: &str, name: &str) -> Option<&'a [u8]> {
    let head = self.green.xattrs.get(path, name).map(Vec::as_slice);
    let Some(fix) = self.fix else {
      return head;
    };
    let (rel, grafted) = self.place(path)?;
    match fix.xattrs.get(&(rel.to_owned(), name.to_owned())) {
      Some(value) => value.as_deref(),
      None => head.filter(|_| grafted),
    }
  }

  /// Whether a file, directory or IPC node stands at `path`: what a mode or an attribute can be set on.
  fn node_stands(&self, path: &str) -> bool {
    self.content(path).is_some() || self.dir(path) || self.special(path).is_some()
  }

  /// Whether a file, directory, symbolic link, hard link or IPC node stands at `path`.
  fn holds_node(&self, path: &str) -> bool {
    self.node_stands(path) || self.symlink(path).is_some() || self.hardlink(path)
  }

  /// Whether a file, directory, symbolic link, hard link or IPC node stands beneath `prefix` and is not in `gone`, as
  /// `merge_rmdir`'s check reads the head: the head's names there and the fix's, each judged as the copy brings it.
  fn has_live_child(&self, prefix: &str, gone: &BTreeSet<String>) -> bool {
    let green = self.green;
    let head = green
      .content
      .prefix(prefix)
      .map(|(path, _)| path)
      .chain(green.dirs.prefix(prefix).map(|(path, _)| path))
      .chain(green.symlinks.prefix(prefix).map(|(path, _)| path))
      .chain(green.hardlinks.prefix(prefix).map(|(path, _)| path))
      .chain(green.specials.prefix(prefix).map(|(path, _)| path));
    let fixed = self
      .fix
      .into_iter()
      .flat_map(|fix| {
        fix
          .files
          .keys()
          .chain(fix.dirs.keys())
          .chain(fix.symlinks.keys())
          .chain(fix.specials.keys())
      })
      .map(|rel| joined(self.source, rel))
      .filter(|path| path.starts_with(prefix));
    head
      .chain(fixed)
      .any(|path| !gone.contains(&path) && self.holds_node(&path))
  }

  /// Whether a key of `root`'s tree (in the source's) stands as the copy brings it: the head's keys the graft brings
  /// there, counted, less those its cuts take, then each key of its fix gained or lost against them. Descents and
  /// counts, never a walk of the tree.
  pub(super) fn holds_tree(&self, root: &str) -> bool {
    let Some(fix) = self.fix else {
      return self.green.tree_held(root);
    };
    let Some((rel_root, grafted)) = self.place(root) else {
      return false;
    };
    let brought = if grafted {
      let cut = fix
        .cuts
        .iter()
        .filter(|cut| in_tree(cut, rel_root))
        .map(|cut| self.green.count_tree(&joined(self.source, cut)))
        .fold(0, usize::saturating_add);
      self.green.count_tree(root).saturating_sub(cut)
    } else {
      0
    };
    let mut counts = (0usize, 0usize);
    self.balance_nodes(fix, rel_root, &mut counts);
    self.balance_metadata(fix, rel_root, &mut counts);
    brought.saturating_add(counts.0) > counts.1
  }

  /// [`SourceAt::holds_tree`]'s tally of the fix's files, IPC nodes and directories in `rel_root`'s tree.
  fn balance_nodes(&self, fix: &CopyFix, rel_root: &str, counts: &mut (usize, usize)) {
    let green = self.green;
    let brought = |rel: &str, held: &dyn Fn(&str) -> bool| {
      grafted_in(fix, rel) && held(&joined(self.source, rel))
    };
    for (rel, bytes) in fix.files.iter().filter(|(rel, _)| in_tree(rel, rel_root)) {
      let held = brought(rel, &|path| green.content.contains_key(path));
      tally(counts, bytes.is_some(), held);
    }
    for (rel, node) in fix
      .specials
      .iter()
      .filter(|(rel, _)| in_tree(rel, rel_root))
    {
      let held = brought(rel, &|path| green.specials.contains_key(path));
      tally(counts, node.is_some(), held);
    }
    for (rel, stood) in fix.dirs.iter().filter(|(rel, _)| in_tree(rel, rel_root)) {
      let held = brought(rel, &|path| green.dirs.contains_key(path));
      tally(counts, *stood, held);
    }
  }

  /// [`SourceAt::holds_tree`]'s tally of the fix's modes, symbolic links and attributes in `rel_root`'s tree.
  fn balance_metadata(&self, fix: &CopyFix, rel_root: &str, counts: &mut (usize, usize)) {
    let green = self.green;
    let brought = |rel: &str, held: &dyn Fn(&str) -> bool| {
      grafted_in(fix, rel) && held(&joined(self.source, rel))
    };
    for (rel, mode) in fix.modes.iter().filter(|(rel, _)| in_tree(rel, rel_root)) {
      let held = brought(rel, &|path| green.modes.contains_key(path));
      tally(counts, mode.is_some(), held);
    }
    for (rel, target) in fix
      .symlinks
      .iter()
      .filter(|(rel, _)| in_tree(rel, rel_root))
    {
      let held = brought(rel, &|path| green.symlinks.contains_key(path));
      tally(counts, target.is_some(), held);
    }
    for ((rel, name), value) in fix
      .xattrs
      .iter()
      .filter(|((rel, _), _)| in_tree(rel, rel_root))
    {
      let held = brought(rel, &|path| green.xattrs.contains_key(path, name));
      tally(counts, value.is_some(), held);
    }
  }
}

/// The copies one chain entry records, for its other effects to read the copied trees by at replay (A-626 part 2): an
/// edited file's bytes before the edit are those its copy brings, a fixed copy's from its fix.
pub(super) struct EntryCopies<'a> {
  copies: Vec<(&'a str, &'a str, Option<&'a CopyFix>)>,
}

impl<'a> EntryCopies<'a> {
  /// The copies `applied` records.
  pub(super) fn of(applied: &'a [Applied]) -> EntryCopies<'a> {
    let copies = applied
      .iter()
      .filter_map(|applied| match applied {
        Applied::CopyTree { dst, from, fix, .. } => {
          Some((dst.as_str(), from.as_str(), fix.as_deref()))
        }
        _ => None,
      })
      .collect();
    EntryCopies { copies }
  }

  /// The source of the innermost of these copies whose tree holds `dst`, as that copy brings it; the head's, for a path
  /// in none.
  pub(super) fn source_at<'g>(&self, green: &'g Green, dst: &str) -> SourceAt<'g>
  where
    'a: 'g,
  {
    let innermost = self
      .copies
      .iter()
      .filter(|(at, ..)| in_tree(dst, at))
      .max_by_key(|(at, ..)| at.len());
    match innermost {
      Some((_, from, fix)) => SourceAt {
        green,
        source: from,
        fix: *fix,
      },
      None => SourceAt {
        green,
        source: "",
        fix: None,
      },
    }
  }
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
  /// Every path a mode or an attribute of the tree is at, once each.
  fn metadata_paths(&self) -> BTreeSet<String> {
    self
      .modes
      .iter()
      .chain(self.xattrs.iter().map(|(path, _)| path))
      .cloned()
      .collect()
  }

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

/// The bytes a copy's record at `dst` holds, charged with the change index: [`appearance_bytes`], and its kept fix's
/// bytes as its chain entry counts them (`chain_entry::fix_bytes`, A-626 part 3).
pub(super) fn record_bytes(dst: &str, record: &Appearance) -> usize {
  appearance_bytes(dst, &record.source).saturating_add(
    record
      .fix
      .as_deref()
      .map_or(0, crate::chain_entry::fix_bytes),
  )
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
  /// either end in a copied tree (Subversion moves by a copy and a removal, which land; A-626), kept apart as what
  /// [`Green::copy_refusal`] refuses ([`CopyRefusal::LinkAcrossCopy`]).
  pub(super) fn split_copies(&mut self) {
    if self.copies.is_empty() {
      return;
    }
    let destinations: Vec<String> = self.copies.keys().cloned().collect();
    let inside = |path: &str| destinations.iter().any(|dst| in_tree(path, dst));
    for (dst, src) in std::mem::take(&mut self.renames) {
      if inside(dst.as_str()) || inside(src.as_str()) {
        self.across_copies.insert(dst);
      } else {
        self.renames.insert(dst, src);
      }
    }
    for (path, target) in std::mem::take(&mut self.hardlinks) {
      if inside(path.as_str()) || inside(target.path.as_str()) {
        self.across_copies.insert(path);
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

  /// Each copy's fix in `r` (A-626 part 2), by destination, computed once a verdict.
  pub(super) fn copy_fixes(&self, r: &Resolved) -> CopyFixes {
    r.copies
      .iter()
      .map(|(dst, spec)| (dst.clone(), self.copy_fix(&spec.source, spec.version)))
      .collect()
  }

  /// The source of the copy `spec` as it brings it, with its fix (`fix`, from [`Green::copy_fixes`]) where it has one.
  fn source_at<'a>(
    &'a self,
    spec: &'a CopySpec,
    fix: Option<&'a Result<Option<CopyFix>, CopyRefusal>>,
  ) -> SourceAt<'a> {
    SourceAt {
      green: self,
      source: &spec.source,
      fix: fix
        .and_then(|fix| fix.as_ref().ok())
        .and_then(Option::as_ref),
    }
  }

  /// The verdict of each copy the increment declares (module docs), its effect or its conflict: a copy into another's
  /// tree is judged against what that copy brings there (A-626), every other against the head.
  pub(super) fn decide_copies(
    &self,
    inc: &Increment,
    (r, fixes): (&Resolved, &CopyFixes),
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    for (dst, spec) in &r.copies {
      match self.decide_copy(inc.base, (dst, spec), (r, fixes)) {
        Ok(effect) => effects.push(effect),
        Err(window) => windows.push(window),
      }
    }
  }

  /// One copy's verdict (module docs); the conflict names the destination, by kind.
  fn decide_copy(
    &self,
    base: u64,
    (dst, spec): (&str, &CopySpec),
    (r, fixes): (&Resolved, &CopyFixes),
  ) -> Result<Effect, ConflictWindow> {
    let fix = match fixes.get(dst) {
      // Refused before the verdict (`Green::copy_refusal`): a guard only, never reached.
      Some(Err(_)) => return Err(Green::window(dst, MergeConflictClass::TypeChanged)),
      Some(Ok(fix)) => fix.as_ref(),
      None => None,
    };
    if let Some(class) = self.destination_refused_in(base, (dst, spec), (r, fixes)) {
      return Err(Green::window(dst, class));
    }
    self.copy_of_source(dst, spec, fix)
  }

  /// Why `dst` takes no copy, if it takes none: against the head ([`Green::destination_refused`]), or, inside another
  /// copy of the increment (A-626, a second copy into a copied tree; Subversion's move within one is a copy and a
  /// removal), against what that copy brings there, which its verdict holds as its source's tree at its version: a tree
  /// there takes the copy only as a replace.
  fn destination_refused_in(
    &self,
    base: u64,
    (dst, spec): (&str, &CopySpec),
    (r, fixes): (&Resolved, &CopyFixes),
  ) -> Option<MergeConflictClass> {
    let Some((outer, outer_spec)) = outer_of(&r.copies, dst) else {
      if dst.is_empty() {
        return Some(MergeConflictClass::TypeChanged);
      }
      return self.destination_refused(base, dst, spec.replace);
    };
    let there = joined(&outer_spec.source, relative(dst, outer).unwrap_or_default());
    let brought = self.source_at(outer_spec, fixes.get(outer));
    (brought.holds_tree(&there) && !spec.replace).then_some(MergeConflictClass::CreateCreate)
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

  /// The source's half of a copy's verdict (module docs), and the copy's effect: refused when the source version is past
  /// the head, when nothing stood in the source's tree at it, and when the source's tree holds a hard link
  /// (`TypeChanged`). A source changed after its version is copied as it stood then, through its fix (A-626 part 2).
  fn copy_of_source(
    &self,
    dst: &str,
    spec: &CopySpec,
    fix: Option<&CopyFix>,
  ) -> Result<Effect, ConflictWindow> {
    let refused = |class| Err(Green::window(dst, class));
    if spec.version > self.head() {
      return refused(MergeConflictClass::TypeChanged);
    }
    let brought = SourceAt {
      green: self,
      source: &spec.source,
      fix,
    };
    if !brought.holds_tree(&spec.source) || self.outside_link_moved(&spec.source, spec.version) {
      return refused(MergeConflictClass::TypeChanged);
    }
    Ok(Effect::CopyTree {
      dst: dst.to_owned(),
      from: spec.source.clone(),
      version: spec.version,
      replace: spec.replace,
      fix: fix.cloned().map(Box::new),
    })
  }

  /// The first copy the increment declares that this piece refuses before any verdict, by its own case (main's ruling
  /// of 2026-11-20: none of them is a concurrent change a rebase could fix, so no conflict class names them), with its
  /// destination: a destination at the root, a source version past the head, nothing in the source's tree at its
  /// version (each a refusal FSFS's commit makes too); the version's values folded away (retention); a hard link in a
  /// source changed since its version, one naming an outside node changed since, and a rename or a hard link declared
  /// across a copied tree's edge (owed work). In the copies' order, the destinations sorted; `None` for an increment this
  /// piece takes to its verdict.
  pub(super) fn copy_refusal(&self, r: &Resolved) -> Option<(String, CopyRefusal)> {
    if let Some(path) = r.across_copies.iter().next() {
      return Some((path.clone(), CopyRefusal::LinkAcrossCopy));
    }
    let fixes = self.copy_fixes(r);
    for (dst, spec) in &r.copies {
      let refused = |case| Some((dst.clone(), case));
      if dst.is_empty() {
        return refused(CopyRefusal::Root);
      }
      if spec.version > self.head() {
        return refused(CopyRefusal::PastHead);
      }
      let fix = match fixes.get(dst) {
        Some(Err(case)) => return refused(*case),
        Some(Ok(fix)) => fix.as_ref(),
        None => None,
      };
      let brought = SourceAt {
        green: self,
        source: &spec.source,
        fix,
      };
      if outer_of(&r.copies, dst).is_none() && !brought.holds_tree(&spec.source) {
        return refused(CopyRefusal::SourceEmpty);
      }
      if self.outside_link_moved(&spec.source, spec.version) {
        return refused(CopyRefusal::OutsideNodeChanged);
      }
    }
    None
  }

  /// Whether a hard link in `source`'s tree names a node outside it whose bytes or node changed after `version`: the
  /// copy gives such a link a node of its own holding the node's bytes ([`Green::copy_link_effects`]), read from the
  /// head, which holds them as they stood at the version only where nothing changed them since.
  fn outside_link_moved(&self, source: &str, version: u64) -> bool {
    self
      .hardlinks
      .prefix(source)
      .filter(|(link, _)| in_tree(link, source))
      .any(|(_, target)| !in_tree(target, source) && self.tree_changed_after(target, version))
  }

  /// The effects a version's copies of trees holding hard links add to it (main's ruling of 2026-11-20, A-586's hard
  /// link in a copied tree): a copy shares every node of its source's tree by reference, a hard link's name among them,
  /// so a copied link still names its node in the source. Names in the copied tree that share a node keep sharing one,
  /// the copy's: each copied link names its node's copy ([`Effect::Hardlink`], which keeps the reverse map of links).
  /// A link whose node lies outside the copied tree gets a node of its own in the copy, holding the node's bytes, mode or
  /// IPC node, its bytes named by the content they hold, never by the outside node. Derived from the head before the
  /// copies land and the version's own effects, as a node's moves are ([`Green::node_moves`]), so a replay derives the
  /// same and the chain entry records none; a copy's verdict accepts no source whose outside nodes changed since its
  /// version ([`Green::outside_link_moved`]), so the head holds what the version held. O(the links in the copied trees).
  pub(super) fn copy_link_effects(&self, effects: &[Effect]) -> Vec<Effect> {
    let mut linked = Vec::new();
    for effect in effects {
      let Effect::CopyTree { dst, from, .. } = effect else {
        continue;
      };
      for (link, target) in self.hardlinks.prefix(from) {
        let Some(rel) = relative(&link, from) else {
          continue;
        };
        let copied = joined(dst, rel);
        match relative(target, from) {
          Some(node) => linked.push(Effect::Hardlink(copied, joined(dst, node))),
          None => self.own_node(copied, target, &mut linked),
        }
      }
    }
    linked
  }

  /// A copied link `copied` given a node of its own holding what the outside node at `target` holds (see
  /// [`Green::copy_link_effects`]): its link removed, then its IPC node or its bytes, and its mode.
  fn own_node(&self, copied: String, target: &str, linked: &mut Vec<Effect>) {
    linked.push(Effect::RemoveHardlink(copied.clone()));
    if let Some(node) = self.specials.get(target) {
      linked.push(Effect::Special(copied, *node));
      return;
    }
    if let Some(mode) = self.modes.get(target) {
      linked.push(Effect::Mode(copied.clone(), *mode));
    }
    let bytes = self.content.get(target).map(<[u8]>::to_vec);
    let source = ContentSource::Copied(target.to_owned(), Vec::new());
    linked.push(Effect::SetContent(copied, bytes, Vec::new(), source));
  }

  /// What a copy of `source` at `version` changes in the head's tree it grafts, so it brings the tree as it stood at the
  /// version (A-626 part 2; FSFS copies from any committed revision): none for a tree unchanged since. The paths the
  /// change index names in the source's tree since the version are the only keys that can differ (a key unnamed since
  /// is unchanged); a copy made since above the source replaced the whole tree, so nothing of the head's is grafted; one
  /// made since inside it is cut. Refused, typed, when the version's values are folded away (below the fold floor or
  /// the checkpoint the green was restored at, [`CopyRefusal::VersionFolded`]) or a hard link stood in the tree at it
  /// ([`CopyRefusal::ChangedSourceLink`]). O(the change index's entries
  /// since the version, and the keys at the paths it names).
  pub(super) fn copy_fix(
    &self,
    source: &str,
    version: u64,
  ) -> Result<Option<CopyFix>, CopyRefusal> {
    if !self.tree_changed_after(source, version) {
      return Ok(None);
    }
    if version < self.folded_below.max(self.deltas_from) {
      return Err(CopyRefusal::VersionFolded);
    }
    let named: BTreeSet<&str> = self
      .changes
      .range(version.saturating_add(1)..)
      .flat_map(|(_, entries)| entries)
      .map(|entry| entry.path.as_str())
      .filter(|path| in_tree(path, source))
      .collect();
    let start_empty = self.replaced_after(source, version);
    let mut fix = CopyFix {
      start_empty,
      ..CopyFix::default()
    };
    if !start_empty {
      fix.cuts = self.cuts_after(source, version, &named);
    }
    for path in named {
      self.fix_nodes(source, (path, version), &mut fix)?;
      self.fix_attributes(source, (path, version), &mut fix);
    }
    Ok(Some(fix))
  }

  /// The trees of the copies made inside `source` after `version` (each a path the change index names, `named`),
  /// relative to the source, the outermost of each nest only.
  fn cuts_after(&self, source: &str, version: u64, named: &BTreeSet<&str>) -> BTreeSet<String> {
    let mut cuts: BTreeSet<String> = BTreeSet::new();
    for path in named {
      let copied = self
        .copies
        .get(*path)
        .is_some_and(|records| records.iter().any(|record| record.version > version));
      let Some(rel) = relative(path, source).filter(|rel| !rel.is_empty()) else {
        continue;
      };
      if copied && !cuts.iter().any(|cut| in_tree(rel, cut)) {
        cuts.retain(|cut| !in_tree(cut, rel));
        cuts.insert(rel.to_owned());
      }
    }
    cuts
  }

  /// [`Green::copy_fix`] of the nodes at `path` (in `source`'s tree): its file, IPC node, directory, mode and symbolic
  /// link at `version`, each recorded where it differs from what the head's graft brings; a hard link then refuses.
  fn fix_nodes(
    &self,
    source: &str,
    (path, version): (&str, u64),
    fix: &mut CopyFix,
  ) -> Result<(), CopyRefusal> {
    if self.hardlink_at(path, version).is_some() {
      return Err(CopyRefusal::ChangedSourceLink);
    }
    let rel = relative(path, source).unwrap_or_default().to_owned();
    let grafted = grafted_in(fix, &rel);
    let then = self.content_ref_at(path, version);
    if then.as_deref() != self.content.get(path).filter(|_| grafted) {
      fix
        .files
        .insert(rel.clone(), then.map(std::borrow::Cow::into_owned));
    }
    let then = self.special_at(path, version);
    if then != self.specials.get(path).copied().filter(|_| grafted) {
      fix.specials.insert(rel.clone(), then);
    }
    let then = self.dir_at(path, version);
    if then != (grafted && self.dirs.contains_key(path)) {
      fix.dirs.insert(rel.clone(), then);
    }
    let then = self.mode_at(path, version);
    if then != self.modes.get(path).copied().filter(|_| grafted) {
      fix.modes.insert(rel.clone(), then);
    }
    let then = self.symlink_at(path, version);
    if then.as_ref() != self.symlinks.get(path).filter(|_| grafted) {
      fix.symlinks.insert(rel, then);
    }
    Ok(())
  }

  /// [`Green::copy_fix`] of the attributes at `path`: each name with a history of its own, recorded at `version` where it
  /// differs from what the head's graft brings. A name with none is unchanged since version 0 or the copy that gave it,
  /// so it reads alike at both ends; one the tree held at the version and lost since has a history (its removal), so
  /// the head's names are never listed, a walk the copy does not take.
  fn fix_attributes(&self, source: &str, (path, version): (&str, u64), fix: &mut CopyFix) {
    let rel = relative(path, source).unwrap_or_default().to_owned();
    let grafted = grafted_in(fix, &rel);
    let from = (path.to_owned(), String::new());
    let names: Vec<String> = self
      .xattr_history
      .range(from..)
      .take_while(|((at, _), _)| at == path)
      .map(|((_, name), _)| name.clone())
      .collect();
    for name in names {
      let then = self.xattr_at(path, &name, version);
      if then.as_ref() != self.xattrs.get(path, &name).filter(|_| grafted) {
        fix.xattrs.insert((rel.clone(), name), then);
      }
    }
  }

  /// The operations on this increment's copied trees (module docs), judged against the copied tree: its copy's source
  /// as the copy brings it.
  pub(super) fn decide_copied(
    &self,
    inc: &Increment,
    (r, fixes): (&Resolved, &CopyFixes),
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    let Some(copied) = r.copied.as_deref() else {
      return;
    };
    let from = |path: &str| self.copied_source((r, fixes), path);
    self.decide_copied_content(inc, copied, &from, effects, windows);
    Green::decide_copied_names(copied, &from, effects, windows);
    Green::decide_copied_metadata(copied, &from, effects, windows);
  }

  /// Where `path`, in a copied tree of `r`, comes from: its innermost copy's source as that copy brings it, and its
  /// place there (`path` itself, outside every copy: never, for the paths the split hands over).
  fn copied_source<'a>(
    &'a self,
    (r, fixes): (&'a Resolved, &'a CopyFixes),
    path: &str,
  ) -> (SourceAt<'a>, String) {
    match innermost(&r.copies, path) {
      Some((dst, spec)) => (
        self.source_at(spec, fixes.get(dst)),
        joined(&spec.source, relative(path, dst).unwrap_or_default()),
      ),
      None => (
        SourceAt {
          green: self,
          source: "",
          fix: None,
        },
        path.to_owned(),
      ),
    }
  }

  /// [`Green::decide_copied`] of creations and edits of files.
  fn decide_copied_content<'a>(
    &'a self,
    inc: &Increment,
    copied: &Resolved,
    from: &impl Fn(&str) -> (SourceAt<'a>, String),
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    for path in &copied.creates {
      let ops = copied.content.get(path).map(Vec::as_slice).unwrap_or(&[]);
      let (brought, at) = from(path);
      let verdict = Green::copied_create(path, (&brought, at.as_str()), ops, inc.post());
      record(effects, windows, verdict);
    }
    for (path, ops) in &copied.content {
      if !copied.creates.contains(path) {
        let (brought, at) = from(path);
        let verdict = Green::copied_edit(path, (&brought, at.as_str()), ops, inc.post());
        record(effects, windows, verdict);
      }
    }
    for (path, node) in &copied.specials {
      let (brought, at) = from(path);
      let effect = Effect::Special(path.clone(), *node);
      record(
        effects,
        windows,
        Green::unless(brought.holds_node(&at), path, effect),
      );
    }
  }

  /// [`Green::decide_copied`] of removals and directories.
  fn decide_copied_names<'a>(
    copied: &Resolved,
    from: &impl Fn(&str) -> (SourceAt<'a>, String),
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    for path in &copied.unlinks {
      let (brought, at) = from(path);
      record(
        effects,
        windows,
        Ok(Green::copied_unlink(path, (&brought, at.as_str()))),
      );
    }
    for path in &copied.mkdirs {
      let (brought, at) = from(path);
      record(
        effects,
        windows,
        Green::copied_mkdir(path, (&brought, at.as_str())),
      );
    }
    let cleared: BTreeSet<String> = copied
      .cleared()
      .iter()
      .map(|path| from(path.as_str()).1)
      .collect();
    for path in &copied.rmdirs {
      let (brought, at) = from(path);
      record(
        effects,
        windows,
        Green::copied_rmdir(path, (&brought, at.as_str()), &cleared),
      );
    }
  }

  /// [`Green::decide_copied`] of modes, symbolic links and attributes.
  fn decide_copied_metadata<'a>(
    copied: &Resolved,
    from: &impl Fn(&str) -> (SourceAt<'a>, String),
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    let absent = |path: &String| {
      let (brought, at) = from(path);
      !brought.node_stands(&at) && !copied.establishes(path)
    };
    for (path, mode) in &copied.modes {
      let effect = Effect::Mode(path.clone(), *mode);
      record(
        effects,
        windows,
        Green::unless_absent(absent(path), path, effect),
      );
    }
    for (path, target) in &copied.symlinks {
      let (brought, at) = from(path);
      let refused = brought.holds_node(&at) && brought.symlink(&at).is_none();
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
      let (brought, at) = from(path);
      let held = brought.xattr(&at, name).is_some();
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

  /// A file created at `path` in a copied tree, where the copy brings nothing (`from`, its place in the source as the
  /// copy brings it).
  fn copied_create(
    path: &str,
    (brought, from): (&SourceAt<'_>, &str),
    ops: &[Op],
    post_state: crate::named::Post<'_>,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if brought.holds_node(from) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    let bytes = apply(&[], ops, post_state);
    let whole = vec![insert_whole(bytes.len() as u64)];
    Ok(Some(Effect::SetContent(
      path.to_owned(),
      Some(bytes),
      whole,
      ContentSource::Created(ops.to_vec()),
    )))
  }

  /// A file the copy brings to `path` (from `from`), edited: the bytes the copy brings with the edits spliced in,
  /// recorded as a rename's edited destination is (A-547), so replay splices them into the same bytes as the verdict did
  /// (a fixed copy's, A-626 part 2, read from its entry: `EntryCopies`).
  fn copied_edit(
    path: &str,
    (brought, from): (&SourceAt<'_>, &str),
    ops: &[Op],
    post_state: crate::named::Post<'_>,
  ) -> Result<Option<Effect>, ConflictWindow> {
    let Some(source) = brought.content(from) else {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    };
    let bytes = apply(source, ops, post_state);
    let whole = vec![insert_whole(bytes.len() as u64)];
    Ok(Some(Effect::SetContent(
      path.to_owned(),
      Some(bytes),
      whole,
      ContentSource::Copied(from.to_owned(), ops.to_vec()),
    )))
  }

  /// A name the copy brings to `path` removed: whatever its source (`from`) is, a file, a symbolic link or an IPC node;
  /// nothing for a name the copy does not bring, as an unlink of an absent name is nothing (`merge_unlink`).
  fn copied_unlink(path: &str, (brought, from): (&SourceAt<'_>, &str)) -> Option<Effect> {
    if brought.content(from).is_some() {
      Some(Effect::RemoveContent(path.to_owned()))
    } else if brought.symlink(from).is_some() {
      Some(Effect::RemoveSymlink(path.to_owned()))
    } else if brought.special(from).is_some() {
      Some(Effect::RemoveSpecial(path.to_owned()))
    } else {
      None
    }
  }

  /// A directory made at `path` in a copied tree: nothing where the copy brings one, a type conflict where it brings
  /// another kind.
  fn copied_mkdir(
    path: &str,
    (brought, from): (&SourceAt<'_>, &str),
  ) -> Result<Option<Effect>, ConflictWindow> {
    if brought.dir(from) {
      return Ok(None);
    }
    if brought.holds_node(from) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    Ok(Some(Effect::MakeDir(path.to_owned())))
  }

  /// A directory the copy brings to `path` removed, once the increment's own removals (`cleared`, in the source's
  /// paths) empty it, as `merge_rmdir` judges one.
  fn copied_rmdir(
    path: &str,
    (brought, from): (&SourceAt<'_>, &str),
    cleared: &BTreeSet<String>,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if !brought.dir(from) {
      if brought.holds_node(from) {
        return Err(Green::window(path, MergeConflictClass::TypeChanged));
      }
      return Ok(None);
    }
    // The directory's own key is no child of it (the root's tree holds the root's own key).
    let mut gone = cleared.clone();
    gone.insert(from.to_owned());
    if brought.has_live_child(&beneath_prefix(from), &gone) {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    Ok(Some(Effect::RemoveDir(path.to_owned())))
  }

  /// The paths whose content ops in this increment's copied trees do not fit what they edit (`super::fits_base`): a
  /// created path edits nothing, an edited one the bytes its copy brings.
  pub(super) fn unfit_copied(
    &self,
    inc: &Increment,
    (r, fixes): (&Resolved, &CopyFixes),
  ) -> Vec<String> {
    let Some(copied) = r.copied.as_deref() else {
      return Vec::new();
    };
    let post_len = inc.post().len();
    copied
      .content
      .iter()
      .filter(|(path, ops)| {
        let base_len = if copied.creates.contains(*path) {
          0
        } else {
          let (brought, from) = self.copied_source((r, fixes), path);
          brought
            .content(&from)
            .map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
        };
        !super::fits_base(ops, base_len, post_len)
      })
      .map(|(path, _)| path.clone())
      .collect()
  }

  /// How many keys `path`'s tree holds in every map (A-626 part 2): descents and the counts their nodes keep, no walk.
  pub(super) fn count_tree(&self, path: &str) -> usize {
    [
      self.content.count_tree(path),
      self.specials.count_tree(path),
      self.dirs.count_tree(path),
      self.modes.count_tree(path),
      self.symlinks.count_tree(path),
      self.hardlinks.count_tree(path),
      self.xattrs.count_tree(path),
    ]
    .into_iter()
    .fold(0, usize::saturating_add)
  }

  /// The room a chain entry for `inc` needs beyond what its increment declares (A-626 part 2): the fix each copy from a
  /// version its source changed after records, as `chain_entry::fix_bytes` counts it. Derived from what this green's
  /// histories hold now, never a fixed allowance: the service adds it to `chain_entry::entry_bound` before the verdict.
  /// Nothing for an increment without such a copy.
  pub fn copy_entry_room(&self, inc: &Increment) -> usize {
    // Most increments declare no copy: they are answered without resolving the document, which the admission and the
    // change index's reservation would each do once more (A-626 part 3).
    if !inc.doc.ops.iter().any(|op| op.kind == OpKind::Copy) {
      return 0;
    }
    let r = Green::resolve(inc);
    self
      .copy_fixes(&r)
      .values()
      .filter_map(|fix| fix.as_ref().ok().and_then(Option::as_ref))
      .map(crate::chain_entry::fix_bytes)
      .fold(0, usize::saturating_add)
  }

  /// Applies the copies among a version's accepted effects, in their destinations' order, an outer copy before one
  /// inside its tree (module docs): every source held first, as the version before this one, whatever a copy of this
  /// version takes away (FSFS copies from a revision, never a transaction); then each destination's tree taken away where
  /// the copy replaces it, the held source grafted there, and a fixed copy's fix applied (A-626 part 2). The paths a
  /// replace took away, for the change index.
  pub(super) fn commit_copies(&mut self, effects: &[Effect], version: u64) -> BTreeSet<String> {
    let mut copies: Vec<Committed<'_>> = effects
      .iter()
      .filter_map(|effect| match effect {
        Effect::CopyTree {
          dst,
          from,
          version,
          replace,
          fix,
        } => Some((
          (dst.as_str(), from.as_str()),
          (*version, *replace),
          fix.as_deref(),
        )),
        _ => None,
      })
      .collect();
    copies.sort_by(|a, b| a.0.0.cmp(b.0.0));
    // A source whose copy starts empty brings nothing of the head's, so it is not held.
    let held: Vec<Option<HeldSource>> = copies
      .iter()
      .map(|((_, src), _, fix)| {
        let grafted = fix.is_none_or(|fix| !fix.start_empty);
        grafted.then(|| self.hold_source(src))
      })
      .collect();
    let mut removed = BTreeSet::new();
    for (((dst, src), (source_version, replace), fix), held) in copies.into_iter().zip(held) {
      if replace {
        removed.extend(self.clear_tree(dst, version));
      }
      self.graft_source(held, (dst, src, source_version), (version, fix));
      if let Some(fix) = fix {
        self.apply_fix(dst, fix);
      }
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
  /// bytes, its `fix` kept in the record and charged with them (A-626 part 3); nothing grafted for a copy that starts
  /// empty (`None`). A map that refuses what the verdict judged possible is counted ([`Green::copies_refused`]), never
  /// silently.
  fn graft_source(
    &mut self,
    held: Option<HeldSource>,
    (dst, src, source_version): (&str, &str, u64),
    (version, fix): (u64, Option<&CopyFix>),
  ) {
    if let Some(held) = held {
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
    }
    self.last_changed.insert(dst.to_owned(), version);
    let record = Appearance {
      version,
      source: src.to_owned(),
      source_version,
      fix: fix.cloned().map(Box::new),
    };
    self.changes_bytes = self
      .changes_bytes
      .saturating_add(record_bytes(dst, &record));
    self.copies.entry(dst.to_owned()).or_default().push(record);
  }

  /// Brings what `dst` holds from the head's source tree to the source's tree at the copied version (A-626 part 2): the
  /// fix's cuts taken away, then each of its values set. None of it is recorded key by key: the copy's record stands for
  /// the tree it brings, its identity entry with it, and a key's first change after the copy records the value it gave.
  fn apply_fix(&mut self, dst: &str, fix: &CopyFix) {
    for cut in &fix.cuts {
      let path = joined(dst, cut);
      self.content.cut_tree(&path);
      self.specials.cut_tree(&path);
      self.dirs.cut_tree(&path);
      self.modes.cut_tree(&path);
      self.symlinks.cut_tree(&path);
      self.hardlinks.cut_tree(&path);
      self.xattrs.cut_tree(&path);
    }
    for (rel, bytes) in &fix.files {
      let path = joined(dst, rel);
      if let Some(bytes) = bytes {
        self.content.insert(&path, bytes.clone());
      } else {
        self.content.remove(&path);
      }
    }
    for (rel, node) in &fix.specials {
      let path = joined(dst, rel);
      if let Some(node) = node {
        self.specials.insert(&path, *node);
      } else {
        self.specials.remove(&path);
      }
    }
    for (rel, stood) in &fix.dirs {
      let path = joined(dst, rel);
      if *stood {
        self.dirs.insert(&path, ());
      } else {
        self.dirs.remove(&path);
      }
    }
    self.apply_fix_metadata(dst, fix);
  }

  /// [`Green::apply_fix`]'s modes, symbolic links and attributes.
  fn apply_fix_metadata(&mut self, dst: &str, fix: &CopyFix) {
    for (rel, mode) in &fix.modes {
      let path = joined(dst, rel);
      if let Some(mode) = mode {
        self.modes.insert(&path, *mode);
      } else {
        self.modes.remove(&path);
      }
    }
    for (rel, target) in &fix.symlinks {
      let path = joined(dst, rel);
      if let Some(target) = target {
        self.symlinks.insert(&path, target.clone());
      } else {
        self.symlinks.remove(&path);
      }
    }
    for ((rel, name), value) in &fix.xattrs {
      let path = joined(dst, rel);
      if let Some(value) = value {
        self.xattrs.insert(&path, name, value.clone());
      } else {
        self.xattrs.remove(&path, name);
      }
    }
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
      .collect();
    for effect in removals {
      self.apply_effect(effect, version, &mut staged);
    }
    // Every mode and attribute left in the tree goes with it, whatever stands at its path (A-618's `drop_metadata`).
    for path in tree.metadata_paths() {
      self.drop_metadata(&path, version);
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
    // Each index is 64 bits (A-621); one the table does not hold names nothing.
    let name = |index: u64| inc.doc.paths.path(index);
    let copies: Vec<(&str, &str)> = inc
      .doc
      .ops
      .iter()
      .filter(|op| op.kind == OpKind::Copy)
      .filter_map(|op| Some((name(op.path)?, name(op.src)?)))
      .collect();
    inc
      .doc
      .ops
      .iter()
      .filter(|op| op.replaces())
      .filter_map(|op| name(op.path))
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
          .map(|path| self.superseded_bytes(path))
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
  /// entry with the value the copy brought it, where it brought one ([`Green::brought_entry`]).
  pub(super) fn copied_entries(&self, key: &IdentityKey, since: u64) -> Vec<Entry> {
    self
      .copies_above_after(key.path(), since)
      .into_iter()
      .filter_map(|(dst, record)| self.brought_entry(key, dst, record))
      .collect()
  }

  /// The entry the copy `record` at `dst` brought `key` (a key of its tree, named at the destination), where it brought
  /// one (A-626 part 3): what its graft brought, the source held at the version before the copy with the copy's fix
  /// applied (`SourceAt`'s reading, at that version): the fix's value where the fix names the key, nothing where the
  /// fix grafts none of the head's there (a copy that starts empty, a cut), else the source's at that version. That
  /// version is never below the fold floor, nor a restored green's checkpoint, for a copy above them: every reader asks
  /// for a copy above them (the identity is brought to its last version first; a branch or a checkpoint is made at the
  /// floor or above). Until A-626 part 3 this read the source's history at the source's version, which a green restored
  /// at a later checkpoint does not hold, so a replica replaying a copy from before its checkpoint hashed otherwise than
  /// its owner (`docs/bugs/2026-11-19-a-copy-from-before-a-checkpoint-hashed-otherwise-on-its-replica.md`).
  pub(super) fn brought_entry(
    &self,
    key: &IdentityKey,
    dst: &str,
    record: &Appearance,
  ) -> Option<Entry> {
    let rel = relative(key.path(), dst)?;
    let from = joined(&record.source, rel);
    let held = record.version.saturating_sub(1);
    let Some(fix) = record.fix.as_deref() else {
      return self.entry_at(key, &from, held);
    };
    match fixed_entry(key, rel, fix) {
      Some(entry) => entry,
      None if grafted_in(fix, rel) => self.entry_at(key, &from, held),
      None => None,
    }
  }

  /// Lets go of the kept fixes of the copies made at or below `floor` (A-626 part 3), their bytes leaving the change
  /// index's charge: nothing reads what such a copy brought any more, since the identity is brought past the floor
  /// before a fold and no branch or checkpoint is made below it.
  pub(super) fn release_kept_fixes(&mut self, floor: u64) {
    let mut released = 0usize;
    for record in self.copies.values_mut().flatten() {
      if record.version <= floor
        && let Some(fix) = record.fix.take()
      {
        released = released.saturating_add(crate::chain_entry::fix_bytes(&fix));
      }
    }
    self.changes_bytes = self.changes_bytes.saturating_sub(released);
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

  /// Every entry the copy `record` at `dst` brought, named at the destination ([`Green::brought_entry`]): every key its
  /// source's tree holds or held, and every key its fix names (one the source held at its version and the green no
  /// longer knows, as a green restored after it). O(everything the green holds): what
  /// [`Green::head_identity_from_scratch`] takes out for each copy, which the running sum never adds (module docs). It
  /// reads each source at the version before its copy, so it holds while the histories reach back to the oldest copy
  /// it counts.
  pub(super) fn copied_tree_entries(&self, dst: &str, record: &Appearance) -> Vec<Entry> {
    let src = record.source.as_str();
    let mut keys: BTreeSet<IdentityKey> = self
      .every_key(src)
      .into_iter()
      .filter_map(|key| {
        let rel = relative(key.path(), src)?;
        Some(key.renamed(&joined(dst, rel)))
      })
      .collect();
    if let Some(fix) = record.fix.as_deref() {
      keys.extend(fixed_keys(dst, fix));
    }
    keys
      .iter()
      .filter_map(|key| self.brought_entry(key, dst, record))
      .collect()
  }

  /// Every key of every dimension the green holds or held in `root`'s tree, current or recorded.
  pub(super) fn every_key(&self, root: &str) -> Vec<IdentityKey> {
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

/// Why this piece refuses a copy before any verdict ([`Green::copy_refusal`]; main's ruling of 2026-11-20): never a
/// conflict, since no concurrent change caused it and no rebase answers it. FSFS's commit refuses the first three too;
/// the fourth follows from what a green retains; the rest are copies this piece does not make yet (owed work, A-586).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyRefusal {
  /// The destination is the root, which always stands (an editor never adds it).
  Root,
  /// The source's version is past the head: a version the green never made.
  PastHead,
  /// Nothing stood in the source's tree at its version.
  SourceEmpty,
  /// The source's version's values are folded away: below the fold floor, or the checkpoint the green was restored at.
  VersionFolded,
  /// A hard link in the source's tree names a node outside it that changed after the version: its copy would read the
  /// node as it stands now (owed).
  OutsideNodeChanged,
  /// The source changed after its version and a hard link stood in its tree then (owed).
  ChangedSourceLink,
  /// A rename or a hard link the increment declares with an end in a copied tree (owed: Subversion has neither).
  LinkAcrossCopy,
}
