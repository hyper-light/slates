//! A verdict's units decided a slice at a time (A-920 piece 3c; A-658's units, `position`): [`Green::decide_units`]'s
//! dimensions, each a phase over the resolved increment's paths, a unit each, taken on from the key or the index after
//! the last one decided, so a step stops between any two units and the next takes it on there. The whole decision is
//! the phases run to their end with nothing ever spent; the units and their order are the same however it is sliced.
//!
//! **Contract.** Between two of its steps neither the green, the increment nor its resolution changes: an import
//! batch's scratch member and its pending work under the landing turn.

use std::collections::BTreeSet;
use std::ops::Bound;

use super::{Green, Increment, MergeConflictClass, Resolved, node_exclusive_paths, position};

/// Where a decision stands between its steps, and what its earlier phases left for its later ones.
#[derive(Debug, Default)]
pub(crate) struct Deciding {
  /// The phase it is in ([`PHASES`]).
  phase: usize,
  /// The last key the phase decided, for a phase over a map or a set: it goes on after it.
  after: Option<String>,
  /// The next index the phase decides, for a phase over a list or the increment's ops.
  at: usize,
  /// The paths the increment removes itself ([`Resolved::cleared`]), once its phase has run.
  cleared: BTreeSet<String>,
  /// The homes whose nodes the increment hands on to links ([`Green::decide_handed_some`]).
  homes: BTreeSet<String>,
  /// The paths the node-exclusive rule has judged ([`Green::decide_node_exclusive_some`]).
  judged: BTreeSet<String>,
  /// Whether [`Deciding::cleared`] is found: by the first unit's first step where it is judged, else by its own phase.
  cleared_found: bool,
  /// The content paths whose ops do not fit what they edit at the work's base, found so far by the first unit's walk.
  unfit: Vec<String>,
  /// The directory removal being decided over steps, its children counted ([`Green::decide_rmdirs_some`]).
  rmdir: Option<RmdirWalk>,
}

/// A directory's removal being decided over steps ([`Green::decide_rmdirs_some`], A-920): of the paths the increment
/// removes itself under it, from the least, those each of the head's node maps holds, counted a path a unit. A child the
/// work does not remove itself is one more key under the directory in a map than its count there.
#[derive(Debug, Default)]
struct RmdirWalk {
  /// Of the paths counted, those each map holds ([`Green::held_under`]'s order).
  cleared: [usize; 5],
  /// The last path counted.
  after: Option<String>,
}

/// What a phase decides from: the increment and its resolution.
struct DecideIn<'d> {
  inc: &'d Increment,
  r: &'d Resolved,
}

/// What a phase decides into: the units, where it stands, and whether its step is spent.
struct Walk<'w> {
  units: &'w mut position::Units,
  deciding: &'w mut Deciding,
  spent: &'w mut dyn FnMut() -> bool,
}

impl Deciding {
  /// One path of what the decision's earlier phases left for its later ones let go: whether one was (A-920 piece 3h,
  /// [`super::Resolved::release_one`]).
  pub(crate) fn release_one(&mut self) -> bool {
    self.cleared.pop_first().is_some()
      || self.homes.pop_first().is_some()
      || self.judged.pop_first().is_some()
      || self.unfit.pop().is_some()
  }
}

impl Walk<'_> {
  /// Whether the step is spent once `key` is decided; where it is, the phase goes on after `key` at the next.
  fn spent_after(&mut self, key: &str) -> bool {
    let spent = (self.spent)();
    if spent {
      self.deciding.after = Some(key.to_owned());
    }
    spent
  }

  /// Whether the step is spent once index `at` is decided; where it is, the phase goes on at the next index.
  fn spent_at(&mut self, at: usize) -> bool {
    self.deciding.at = at.saturating_add(1);
    (self.spent)()
  }

  /// The keys after the last one decided: what a phase over a map or a set takes on.
  fn after(&self) -> (Bound<&str>, Bound<&str>) {
    let after = self.deciding.after.as_deref();
    (
      after.map_or(Bound::Unbounded, Bound::Excluded),
      Bound::Unbounded,
    )
  }
}

/// A phase: whether it ended; where it did not, its step is spent.
type Phase = fn(&Green, &DecideIn<'_>, &mut Walk<'_>) -> bool;

/// The phases, in the order the whole verdict pushes its units (A-658, `position`).
const PHASES: [Phase; 17] = [
  Green::decide_first,
  Green::decide_copies_phase,
  Green::decide_specials_some,
  Green::decide_cleared,
  Green::decide_handed_some,
  Green::decide_renames_some,
  Green::decide_creates_some,
  Green::decide_unlinks_some,
  Green::decide_content_some,
  Green::decide_mkdirs_some,
  Green::decide_rmdirs_some,
  Green::decide_modes_some,
  Green::decide_symlinks_some,
  Green::decide_hardlinks_some,
  Green::decide_xattr_sets_some,
  Green::decide_xattr_removes_some,
  Green::decide_node_exclusive_some,
];

/// The files the work removes or replaces: the first unit's footprint names the links to them (A-669).
fn leaving(r: &Resolved) -> Vec<&str> {
  r.unlinks
    .iter()
    .chain(r.renames.keys())
    .map(String::as_str)
    .collect()
}

impl Green {
  /// Decides every dimension of an increment unit by unit into `units` (A-658, `position`): each unit one verdict
  /// function over its own paths, in the order the whole verdict pushes its effects and windows, so the verdict reached
  /// at one head is reached at a later one by judging again only the units the versions since changed.
  pub(super) fn decide_units(&self, inc: &Increment, r: &Resolved, units: &mut position::Units) {
    let mut deciding = Deciding::default();
    while !self.decide_some(inc, r, (units, &mut deciding), &mut || false) {}
  }

  /// [`Green::decide_units`] taken on from `deciding`, a unit at least, until `spent`: whether every unit is decided.
  pub(super) fn decide_some(
    &self,
    inc: &Increment,
    r: &Resolved,
    (units, deciding): (&mut position::Units, &mut Deciding),
    spent: &mut dyn FnMut() -> bool,
  ) -> bool {
    let from = DecideIn { inc, r };
    let mut walk = Walk {
      units,
      deciding,
      spent,
    };
    while let Some(phase) = PHASES.get(walk.deciding.phase) {
      if !phase(self, &from, &mut walk) {
        return false;
      }
      walk.deciding.phase = walk.deciding.phase.saturating_add(1);
      walk.deciding.after = None;
      walk.deciding.at = 0;
    }
    true
  }

  /// What the work and its base decide: an operation no verdict applies, an edit that does not fit what it edits. The
  /// head decides one thing of it, the file an edit declared at a link edits when the work hands that file's node on to
  /// the link (A-669), which turns only with the links naming the files the work removes or replaces. One unit, judged
  /// over steps (A-920 3j): its first asks whether it is judged at all ([`position::Units::judges`]) and finds the paths
  /// the work removes itself; then each content path's fit to its base is a unit of the walk ([`Green::content_fits`]),
  /// the unfit kept; and the unit is pushed whole when the walk ends, so the units are the same however it is sliced.
  fn decide_first(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let r = from.r;
    if walk.deciding.at == 0 {
      walk.deciding.at = 1;
      if !walk
        .units
        .judges(&position::Footprint::links_naming(&leaving(r)))
      {
        return self.first_unit_pushed(from, walk);
      }
      walk.deciding.cleared = r.cleared();
      walk.deciding.cleared_found = true;
      if (walk.spent)() {
        return false;
      }
    }
    for (path, ops) in r.content.range::<str, _>(walk.after()) {
      if !self.content_fits(
        from.inc,
        r,
        &walk.deciding.cleared,
        (path.as_str(), ops.as_slice()),
      ) {
        walk.deciding.unfit.push(path.clone());
      }
      if walk.spent_after(path) {
        return false;
      }
    }
    self.first_unit_pushed(from, walk)
  }

  /// The first unit pushed ([`Green::decide_first`]): its windows the work's invalid operations and its unfit content,
  /// as found by its walk; judged whole here should the units judge it where its first step found they would not.
  fn first_unit_pushed(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let (inc, r) = (from.inc, from.r);
    let unfit = core::mem::take(&mut walk.deciding.unfit);
    let walked = walk.deciding.cleared_found;
    walk
      .units
      .unit(position::Footprint::links_naming(&leaving(r)), |_, sink| {
        let unfit = if walked {
          unfit
        } else {
          let cleared = r.cleared();
          r.content
            .iter()
            .filter(|(path, ops)| {
              !self.content_fits(inc, r, &cleared, (path.as_str(), ops.as_slice()))
            })
            .map(|(path, _)| path.clone())
            .collect()
        };
        sink.windows(
          r.invalid
            .iter()
            .chain(unfit.iter())
            .map(|path| Green::window(path, MergeConflictClass::TypeChanged)),
        );
      });
    true
  }

  /// Copies first, and what lands on their trees after them (A-586): their effects lead, as commit applies them. One
  /// unit, whole.
  fn decide_copies_phase(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    if !from.r.copies.is_empty() {
      walk
        .units
        .copies(|sink| self.decide_copy_unit(from.inc, from.r, sink));
    }
    true
  }

  /// Each IPC node the increment makes, a unit each.
  fn decide_specials_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let r = from.r;
    for (path, node) in r.specials.range::<str, _>(walk.after()) {
      walk.units.unit(position::Footprint::path(path), |_, sink| {
        sink.of(self.merge_special(path, from.inc.base, *node))
      });
      if walk.spent_after(path) {
        return false;
      }
    }
    true
  }

  /// The paths the increment removes itself: a rename over a file and a directory's removal read them.
  fn decide_cleared(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    if !walk.deciding.cleared_found {
      walk.deciding.cleared = from.r.cleared();
      walk.deciding.cleared_found = true;
    }
    true
  }

  /// The writes a node takes to the link it passes to (A-669), a unit each: each link the increment declares writes at
  /// whose node's home it removes or replaces ([`Green::handed_home`]), judged and handed on ([`Green::merge_handed`]).
  /// Notes the homes handed on, which the removals read ([`Green::decide_renames_some`],
  /// [`Green::decide_unlinks_some`]); the content's unit for each such link is empty. Whether a link stands at a path
  /// the work writes is the head's, while which units there are is the work's alone (A-658, `position`), so a work that
  /// removes or replaces a file has a unit for every path it writes and declares nothing else of, empty where no node
  /// passes: its footprint is the path, and the home its node passes from when one does.
  fn decide_handed_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let (inc, r) = (from.inc, from.r);
    if r.unlinks.is_empty() && r.renames.is_empty() {
      return true; // nothing removed or replaced: no node passes on
    }
    for (heir, ops) in r.content.range::<str, _>(walk.after()) {
      if !r.declares_more_than_writes(heir, &walk.deciding.cleared) {
        let home = self.handed_home(r, heir, &walk.deciding.cleared);
        walk.deciding.homes.extend(home.map(str::to_owned));
        let footprint = home.map_or(position::Footprint::path(heir), |home| {
          position::Footprint::pair(heir, home)
        });
        walk.units.unit(footprint, |_, sink| {
          let Some(home) = home else {
            return;
          };
          let unlinked = r.unlinks.contains(home);
          match self.merge_handed((heir, ops), (home, unlinked), (inc.base, inc.post())) {
            Ok(effects) => sink.effect_list(effects),
            Err(window) => sink.windows([window]),
          }
        });
      }
      if walk.spent_after(heir) {
        return false;
      }
    }
    true
  }

  /// The content dimension's renames, a unit each. A replacement by a rename of a file whose node a link the work's base
  /// never had now names is refused before it is merged, unless the increment hands the node on itself (the homes,
  /// [`Green::decide_handed_some`]): rename against rename for a replaced destination, as a change the work did not see
  /// is ([`Green::leaves_to_an_unseen_link`], A-670). Such a unit reads the links naming its file, whose changes are
  /// recorded at the links' own names, never at the file's, so its footprint names them (A-658, `position`).
  fn decide_renames_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let (inc, r) = (from.inc, from.r);
    for (dst, source) in r.renames.range::<str, _>(walk.after()) {
      let ops = r.content.get(dst).map(Vec::as_slice).unwrap_or(&[]);
      let replaced = [dst.as_str()];
      let footprint = position::Footprint::pair(dst, source).and_links_naming(&replaced);
      let (cleared, homes) = (&walk.deciding.cleared, &walk.deciding.homes);
      walk.units.unit(footprint, |_, sink| {
        let unseen =
          !cleared.contains(dst) && self.leaves_to_an_unseen_link(dst, inc.base, (r, homes));
        let merged = if unseen {
          Err(Green::window(dst, MergeConflictClass::RenameRename))
        } else {
          let moved = (inc.base, inc.post());
          self.merge_rename((dst, ops), moved, (source.as_str(), cleared), sink.effects)
        };
        if let Err(window) = merged {
          sink.windows.push(window);
        }
      });
      if walk.spent_after(dst) {
        return false;
      }
    }
    true
  }

  /// The content dimension's creates, a unit each.
  fn decide_creates_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let (inc, r) = (from.inc, from.r);
    for path in r.creates.range::<str, _>(walk.after()) {
      walk.units.unit(position::Footprint::path(path), |_, sink| {
        let content = r.content.get(path).map(Vec::as_slice).unwrap_or(&[]);
        sink.of(self.merge_create(path, (content, inc.post())))
      });
      if walk.spent_after(path) {
        return false;
      }
    }
    true
  }

  /// The content dimension's unlinks, a unit each. A removal of a file whose node a link the work's base never had now
  /// names is refused before it is merged, unless the increment hands the node on itself: delete against modify, as a
  /// change the work did not see is ([`Green::leaves_to_an_unseen_link`], A-670); its footprint names the links.
  fn decide_unlinks_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let (base, r) = (from.inc.base, from.r);
    for path in r.unlinks.range::<str, _>(walk.after()) {
      let removed = [path.as_str()];
      let footprint = position::Footprint::path(path).and_links_naming(&removed);
      let homes = &walk.deciding.homes;
      walk.units.unit(footprint, |_, sink| {
        sink.of(if self.leaves_to_an_unseen_link(path, base, (r, homes)) {
          Err(Green::window(path, MergeConflictClass::DeleteModify))
        } else {
          self.merge_unlink(path, base)
        })
      });
      if walk.spent_after(path) {
        return false;
      }
    }
    true
  }

  /// The content dimension's edits, a unit each: every path the increment edits but neither creates nor renames onto,
  /// whose content goes with that effect.
  fn decide_content_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let (inc, r) = (from.inc, from.r);
    for (path, ops) in r.content.range::<str, _>(walk.after()) {
      if !r.creates.contains(path) && !r.renames.contains_key(path) {
        let cleared = &walk.deciding.cleared;
        walk
          .units
          .unit(position::Footprint::path(path), |placed, sink| {
            // A link's writes go with the node handed on to it (`decide_handed_some`); only a change at the link turns
            // that.
            if self.handed_home(r, path, cleared).is_some() {
              return;
            }
            self.merge_content(inc, path, (ops, placed), sink);
          });
      }
      if walk.spent_after(path) {
        return false;
      }
    }
    true
  }

  /// The directories the increment makes, a unit each.
  fn decide_mkdirs_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    for path in from.r.mkdirs.range::<str, _>(walk.after()) {
      walk.units.unit(position::Footprint::path(path), |_, sink| {
        sink.of(self.merge_mkdir(path))
      });
      if walk.spent_after(path) {
        return false;
      }
    }
    true
  }

  /// The directories the increment removes, a unit each over the directory's tree.
  ///
  /// A removal's children are counted, never walked whole (A-920: a directory of 9,000 files the work removed walked them
  /// all in one unit, 4.96 ms debug): the head's keys under the directory in each map, a descent each; and the paths the
  /// work removes itself under it, a path a unit, counted where each map holds them. A child survives exactly where a
  /// map holds more keys under the directory than its count, so the removal is delete against modify there.
  fn decide_rmdirs_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let base = from.inc.base;
    // Taken on after the last directory decided; a removal whose children were being counted is taken on again there.
    let after = walk.deciding.after.clone();
    let from_after = after.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
    for path in from
      .r
      .rmdirs
      .range::<str, _>((from_after, Bound::Unbounded))
    {
      let Some(decided) = self.rmdir_decided(path, base, walk) else {
        return false;
      };
      walk
        .units
        .unit(position::Footprint::directory(path), |_, sink| {
          sink.of(decided)
        });
      if walk.spent_after(path) {
        return false;
      }
    }
    true
  }

  /// The removal of the directory at `path` decided ([`Green::decide_rmdirs_some`]), its children counted from where the
  /// last step left them: none while the step is spent before they all are.
  fn rmdir_decided(
    &self,
    path: &str,
    base: u64,
    walk: &mut Walk<'_>,
  ) -> Option<Result<Option<super::Effect>, super::ConflictWindow>> {
    if walk.deciding.rmdir.is_none() {
      if let Some(decided) = self.rmdir_before_children(path, base) {
        return Some(decided);
      }
      walk.deciding.rmdir = Some(RmdirWalk::default());
    }
    let prefix = format!("{path}/");
    if !self.rmdir_children_counted(&prefix, walk) {
      return None;
    }
    let counted = walk.deciding.rmdir.take().unwrap_or_default();
    let live = self
      .held_under(&prefix)
      .iter()
      .zip(counted.cleared)
      .any(|(held, cleared)| *held > cleared);
    Some(if live {
      Err(Green::window(path, MergeConflictClass::DeleteModify))
    } else {
      Ok(Some(super::Effect::RemoveDir(path.to_owned())))
    })
  }

  /// The paths the work removes itself under `prefix` counted where each of the head's maps holds them, a path a unit
  /// from the last counted: whether all are, else the step is spent.
  fn rmdir_children_counted(&self, prefix: &str, walk: &mut Walk<'_>) -> bool {
    let Walk {
      deciding, spent, ..
    } = walk;
    let Deciding { cleared, rmdir, .. } = &mut **deciding;
    let Some(counting) = rmdir.as_mut() else {
      return true;
    };
    let mut counted = 0u64;
    let all = loop {
      let from = counting
        .after
        .as_deref()
        .map_or(Bound::Included(prefix), Bound::Excluded);
      let Some(child) = cleared
        .range::<str, _>((from, Bound::Unbounded))
        .next()
        .filter(|child| child.starts_with(prefix))
      else {
        break true;
      };
      for (count, held) in counting.cleared.iter_mut().zip(self.held_at(child)) {
        if held {
          *count = count.saturating_add(1);
        }
      }
      counting.after = Some(child.clone());
      counted = counted.saturating_add(1);
      if spent() {
        break false;
      }
    };
    self.rmdir_children_noted(counted);
    all
  }

  /// The modes the increment sets, a unit each.
  fn decide_modes_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let (base, r) = (from.inc.base, from.r);
    for (path, mode) in r.modes.range::<str, _>(walk.after()) {
      walk.units.unit(position::Footprint::path(path), |_, sink| {
        sink.of(self.merge_setmode(path, base, *mode, r.establishes(path)))
      });
      if walk.spent_after(path) {
        return false;
      }
    }
    true
  }

  /// The symbolic links the increment makes, a unit each.
  fn decide_symlinks_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let base = from.inc.base;
    for (path, target) in from.r.symlinks.range::<str, _>(walk.after()) {
      walk.units.unit(position::Footprint::path(path), |_, sink| {
        sink.of(self.merge_symlink(path, base, target))
      });
      if walk.spent_after(path) {
        return false;
      }
    }
    true
  }

  /// The hard links the increment makes, a unit each; a hard link's unit reads the node it names (A-646): a path at
  /// the base or one the work makes; none when the document names a node the work does not make, or links that never
  /// reach one.
  fn decide_hardlinks_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let (base, r) = (from.inc.base, from.r);
    for (path, target) in r.hardlinks.range::<str, _>(walk.after()) {
      let node = r.link_node(target);
      let footprint = node.map_or(position::Footprint::path(path), |(home, _)| {
        position::Footprint::pair(path, home)
      });
      walk.units.unit(footprint, |_, sink| {
        sink.of(match node {
          Some(node) => self.merge_hardlink(path, base, node),
          None => Err(Green::window(path, MergeConflictClass::TypeChanged)),
        })
      });
      if walk.spent_after(path) {
        return false;
      }
    }
    true
  }

  /// The extended attributes the increment sets, a unit each, in the order it declares them.
  fn decide_xattr_sets_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let (base, r) = (from.inc.base, from.r);
    let mut at = walk.deciding.at;
    while let Some((path, name, value)) = r.xattr_sets.get(at) {
      walk.units.unit(position::Footprint::path(path), |_, sink| {
        sink.of(self.merge_set_xattr(path, name, base, value, r.establishes(path)))
      });
      if walk.spent_at(at) {
        return false;
      }
      at = at.saturating_add(1);
    }
    true
  }

  /// The extended attributes the increment removes, a unit each, in the order it declares them.
  fn decide_xattr_removes_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let (base, r) = (from.inc.base, from.r);
    let mut at = walk.deciding.at;
    while let Some((path, name)) = r.xattr_removes.get(at) {
      walk.units.unit(position::Footprint::path(path), |_, sink| {
        sink.of(self.merge_remove_xattr(path, name, base, r.establishes(path)))
      });
      if walk.spent_at(at) {
        return false;
      }
      at = at.saturating_add(1);
    }
    true
  }

  /// The node-exclusive rule (§4.17.4 item 3, rules 1 and 2; A-542): a path an operation marked node-exclusive changes,
  /// a move's source among them, that changed in any dimension after the increment's base is a conflict, however
  /// disjoint the bytes and however identical the change, as FSFS's commit-time merge judges a node (`tree.c`
  /// `merge()`: a file changed on both sides fails, "even an identical replacement", and two additions are "not
  /// auto-mergeable"). The dimension verdicts have run and added their own windows; this pass refuses only what they
  /// would accept. Each path is one lookup of its last change in the change index (`latest_change`), and is judged once,
  /// a unit of its own; an op a unit of the walk.
  fn decide_node_exclusive_some(&self, from: &DecideIn<'_>, walk: &mut Walk<'_>) -> bool {
    let inc = from.inc;
    let mut at = walk.deciding.at;
    while let Some(op) = inc.doc.ops.get(at) {
      if op.is_node_exclusive() {
        self.decide_node_exclusive_op(inc, op, walk);
      }
      if walk.spent_at(at) {
        return false;
      }
      at = at.saturating_add(1);
    }
    true
  }

  /// [`Green::decide_node_exclusive_some`]'s each node-exclusive op: each path it names not judged yet, a unit.
  fn decide_node_exclusive_op(
    &self,
    inc: &Increment,
    op: &crate::ops_doc::Op,
    walk: &mut Walk<'_>,
  ) {
    for (index, class) in node_exclusive_paths(op) {
      let Some(path) = inc.doc.paths.path(index) else {
        continue;
      };
      if walk.deciding.judged.insert(path.to_owned()) {
        walk.units.unit(position::Footprint::path(path), |_, sink| {
          let changed = self.changed_after(path, inc.base);
          sink.windows(changed.then(|| Green::window(path, class)));
        });
      }
    }
  }
}
