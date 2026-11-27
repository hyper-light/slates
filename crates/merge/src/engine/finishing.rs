//! A resolution's end taken a unit a step (A-920: move's judge ran it whole, 5.86 ms of one step at 9,000 paths, debug;
//! main's ruling of 2026-11-27 02:03, any loop over user-scaled data in one unit is a defect).
//!
//! **What.** [`Resolved::finish_resolving`] marks invalid each IPC node the increment also makes or removes as another
//! kind, then hands every operation on a path in one of the increment's copies' trees to [`Resolved::copied`], judged
//! against the copied tree (`copy` module docs): renames and hard links reaching a copy's tree are marked across copies,
//! the rest of each container split between the copied tree and what stays. Whole, each container was taken out and
//! partitioned in one call, and whether a path lay in a copy's tree was a test of it against every copy's destination.
//!
//! [`Finishing`] takes the same steps an entry a unit: the IPC nodes checked from the last one checked, then each
//! container drained in the whole split's order, every entry routed back into the resolution or into the copied tree.
//! A path lies in a copy's tree exactly where the root or one of the path's own ancestors (itself included) is a copy's
//! destination: a lookup a level of the path ([`inside_copies`]), never a walk of the copies. The resolution is the
//! whole end's: each map and set keyed alike, each list in its order.

use std::collections::{BTreeMap, BTreeSet, btree_map, btree_set};
use std::ops::Bound;

use super::copy::{CopySpec, ancestors};
use super::{LinkTarget, Op, Resolved, SpecialNode};

/// A resolution's end being taken, a unit a step ([`Finishing::step`]).
#[derive(Default)]
pub(super) struct Finishing {
  /// The part being taken.
  stage: Stage,
  /// The last IPC node checked.
  after: Option<String>,
  /// What the copies' trees take, made up an entry a unit.
  copied: Resolved,
  /// Each container being drained, taken out of the resolution as its part begins.
  drains: Drains,
}

/// The containers a resolution's end drains, each once its part begins.
#[derive(Default)]
struct Drains {
  renames: Option<btree_map::IntoIter<String, String>>,
  hardlinks: Option<btree_map::IntoIter<String, LinkTarget>>,
  content: Option<btree_map::IntoIter<String, Vec<Op>>>,
  creates: Option<btree_set::IntoIter<String>>,
  specials: Option<btree_map::IntoIter<String, SpecialNode>>,
  unlinks: Option<btree_set::IntoIter<String>>,
  mkdirs: Option<btree_set::IntoIter<String>>,
  rmdirs: Option<btree_set::IntoIter<String>>,
  modes: Option<btree_map::IntoIter<String, u32>>,
  symlinks: Option<btree_map::IntoIter<String, String>>,
  xattr_sets: Option<std::vec::IntoIter<(String, String, Vec<u8>)>>,
  xattr_removes: Option<std::vec::IntoIter<(String, String)>>,
}

/// The parts of a resolution's end, in the whole end's order.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Stage {
  /// Each IPC node the increment also makes or removes as another kind, marked invalid.
  #[default]
  Specials,
  Renames,
  Hardlinks,
  Content,
  Creates,
  SplitSpecials,
  Unlinks,
  Mkdirs,
  Rmdirs,
  Modes,
  Symlinks,
  XattrSets,
  XattrRemoves,
  Done,
}

impl Stage {
  /// The part after this one.
  fn next(self) -> Stage {
    match self {
      Stage::Specials => Stage::Renames,
      Stage::Renames => Stage::Hardlinks,
      Stage::Hardlinks => Stage::Content,
      Stage::Content => Stage::Creates,
      Stage::Creates => Stage::SplitSpecials,
      Stage::SplitSpecials => Stage::Unlinks,
      Stage::Unlinks => Stage::Mkdirs,
      Stage::Mkdirs => Stage::Rmdirs,
      Stage::Rmdirs => Stage::Modes,
      Stage::Modes => Stage::Symlinks,
      Stage::Symlinks => Stage::XattrSets,
      Stage::XattrSets => Stage::XattrRemoves,
      Stage::XattrRemoves | Stage::Done => Stage::Done,
    }
  }
}

impl Finishing {
  /// One entry of `r`'s end taken: whether the end is whole, the copied tree's operations handed to `r.copied` (where
  /// the increment copies anything).
  pub(super) fn step(&mut self, r: &mut Resolved) -> bool {
    loop {
      if self.stage == Stage::Done {
        return true;
      }
      if self.unit(r) {
        return false;
      }
      self.stage = self.stage.next();
      // Where the increment copies nothing, nothing is split: its end is the IPC nodes' check alone.
      if self.stage == Stage::Renames && r.copies.is_empty() {
        self.stage = Stage::Done;
      }
      if self.stage == Stage::Done && !r.copies.is_empty() {
        r.copied = Some(Box::new(core::mem::take(&mut self.copied)));
      }
    }
  }

  /// One entry of the part under way: whether there was one.
  fn unit(&mut self, r: &mut Resolved) -> bool {
    let (copies, d, c) = (&r.copies, &mut self.drains, &mut self.copied);
    let inside = |path: &str| inside_copies(copies, path);
    match self.stage {
      Stage::Specials => self.special_checked(r),
      Stage::Renames => route_across(
        &mut d.renames,
        (&mut r.renames, &mut r.across_copies),
        |dst, src: &String| inside(dst) || inside(src),
      ),
      Stage::Hardlinks => route_across(
        &mut d.hardlinks,
        (&mut r.hardlinks, &mut r.across_copies),
        |path, target: &LinkTarget| inside(path) || inside(&target.path),
      ),
      Stage::Content => route_map(&mut d.content, (&mut r.content, &mut c.content), inside),
      Stage::Creates => route_set(&mut d.creates, (&mut r.creates, &mut c.creates), inside),
      Stage::SplitSpecials => {
        route_map(&mut d.specials, (&mut r.specials, &mut c.specials), inside)
      }
      Stage::Unlinks => route_set(&mut d.unlinks, (&mut r.unlinks, &mut c.unlinks), inside),
      Stage::Mkdirs => route_set(&mut d.mkdirs, (&mut r.mkdirs, &mut c.mkdirs), inside),
      Stage::Rmdirs => route_set(&mut d.rmdirs, (&mut r.rmdirs, &mut c.rmdirs), inside),
      Stage::Modes => route_map(&mut d.modes, (&mut r.modes, &mut c.modes), inside),
      Stage::Symlinks => route_map(&mut d.symlinks, (&mut r.symlinks, &mut c.symlinks), inside),
      Stage::XattrSets => route_list(
        &mut d.xattr_sets,
        (&mut r.xattr_sets, &mut c.xattr_sets),
        |(path, _, _)| inside(path),
      ),
      Stage::XattrRemoves => route_list(
        &mut d.xattr_removes,
        (&mut r.xattr_removes, &mut c.xattr_removes),
        |(path, _)| inside(path),
      ),
      Stage::Done => false,
    }
  }

  /// [`Stage::Specials`]: the next IPC node after the last checked marked invalid where the increment also makes or
  /// removes another kind at its path: whether there was one.
  fn special_checked(&mut self, r: &mut Resolved) -> bool {
    let from = self
      .after
      .as_deref()
      .map_or(Bound::Unbounded, Bound::Excluded);
    let Some(path) = r
      .specials
      .range::<str, _>((from, Bound::Unbounded))
      .next()
      .map(|(path, _)| path.clone())
    else {
      return false;
    };
    if r.creates.contains(&path)
      || r.content.contains_key(&path)
      || r.mkdirs.contains(&path)
      || r.symlinks.contains_key(&path)
      || r.hardlinks.contains_key(&path)
      || r.renames.contains_key(&path)
      || r.unlinks.contains(&path)
      || r.rmdirs.contains(&path)
    {
      r.invalid.insert(path.clone());
    }
    self.after = Some(path);
    true
  }
}

/// Whether `path` lies in one of `copies`' trees: the root or one of the path's own ancestors (itself included) is a
/// copy's destination, a lookup a level of the path.
fn inside_copies(copies: &BTreeMap<String, CopySpec>, path: &str) -> bool {
  copies.contains_key("") || ancestors(path).any(|prefix| copies.contains_key(prefix))
}

/// The next entry of `drain` (taken out of `kept` as its part begins) put back into `kept`, or into `copied` where
/// `inside` holds its path: whether there was one.
fn route_map<V>(
  drain: &mut Option<btree_map::IntoIter<String, V>>,
  (kept, copied): (&mut BTreeMap<String, V>, &mut BTreeMap<String, V>),
  inside: impl Fn(&str) -> bool,
) -> bool {
  let entries = drain.get_or_insert_with(|| core::mem::take(kept).into_iter());
  let Some((path, value)) = entries.next() else {
    return false;
  };
  if inside(&path) {
    copied.insert(path, value);
  } else {
    kept.insert(path, value);
  }
  true
}

/// The next entry of `drain` (taken out of `kept` as its part begins) put back into `kept`, or its path marked across
/// copies where `reaches` holds (a rename or a hard link reaching a copy's tree): whether there was one.
fn route_across<V>(
  drain: &mut Option<btree_map::IntoIter<String, V>>,
  (kept, across): (&mut BTreeMap<String, V>, &mut BTreeSet<String>),
  reaches: impl Fn(&str, &V) -> bool,
) -> bool {
  let entries = drain.get_or_insert_with(|| core::mem::take(kept).into_iter());
  let Some((path, value)) = entries.next() else {
    return false;
  };
  if reaches(&path, &value) {
    across.insert(path);
  } else {
    kept.insert(path, value);
  }
  true
}

/// [`route_map`] for a set of paths.
fn route_set(
  drain: &mut Option<btree_set::IntoIter<String>>,
  (kept, copied): (&mut BTreeSet<String>, &mut BTreeSet<String>),
  inside: impl Fn(&str) -> bool,
) -> bool {
  let paths = drain.get_or_insert_with(|| core::mem::take(kept).into_iter());
  let Some(path) = paths.next() else {
    return false;
  };
  if inside(&path) {
    copied.insert(path);
  } else {
    kept.insert(path);
  }
  true
}

/// [`route_map`] for a list, each entry kept in its order, `inside` read of the entry.
fn route_list<T>(
  drain: &mut Option<std::vec::IntoIter<T>>,
  (kept, copied): (&mut Vec<T>, &mut Vec<T>),
  inside: impl Fn(&T) -> bool,
) -> bool {
  let items = drain.get_or_insert_with(|| core::mem::take(kept).into_iter());
  let Some(item) = items.next() else {
    return false;
  };
  if inside(&item) {
    copied.push(item);
  } else {
    kept.push(item);
  }
  true
}
