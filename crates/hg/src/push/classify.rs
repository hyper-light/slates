//! Which lines, bookmarks and phases a verified push moves (A-657; §4.17.3 decisions 1 to 3 and 6, "Push" steps 3 and
//! 7): the push's new changesets placed on the lines and bookmarks the push read, its phase moves as changes of the
//! draft phase's roots, its pushkeys answered, and the change in heads its `reply:changegroup` gives, each as 7.2 does
//! it (read 2026-11-19 in the wheel of Mercurial 7.2 that uv installs, on the Mac Studio) \[C\]: `branchmap.py`,
//! `changegroup.py` `apply`, `phases.py` (`registernew`, `advanceboundary`, `updatephases`, `pushphase`),
//! `bookmarks.py` (`binarydecode`, `isdivergent`, `pushbookmark`, `bmstore.applychanges`) and
//! `bundle2_part_handlers.py`.
//!
//! **Lines.** A branch head is a changeset no descendant of which is on its branch, through any path (7.2's branchmap
//! drops a head that is an ancestor of a new changeset of its branch; checked with stock 7.2 on a branch that leaves
//! and comes back). After a push, a branch's heads are its heads before it and its new changesets, less every one of
//! them a new changeset of the branch descends from. A line whose head is still a head stays. Each new head is matched
//! to a line whose head it descends from (an empty line's head is the null node, which every changeset descends from),
//! by a maximum matching (augmenting paths, the new heads in the order they arrived and each one's lines in their
//! numbers' order, so the matching is the same on every holder): a matched line moves to its head, which costs its own
//! log alone; a new head no line is left for makes a line, numbered the lowest number its branch does not use; a line
//! whose head is no longer a head and that no new head took is retired, its head merged into one another line took.
//! Only a made or retired line touches the repository's names log, and the matching makes as few of them as the push
//! allows. A made line is made from the version of a line its head descends from, or else from a line holding the first
//! changeset the repository held under the new head's first parents, or a line of that changeset's branch: whichever
//! base is chosen, its work replaces exactly what differs, so the choice costs only the work's size.
//!
//! **Phases.** Only public and draft live in a served view, so the phases are the draft phase's roots, phase 1. The
//! parts are taken in their stream order, as 7.2's handlers take them: the changegroup makes every changeset it brings
//! draft (`registernew`), or on a publishing repository public together with every draft ancestor of the changesets it
//! carried, held ones included (`advanceboundary(public, duprevs + added)`); a `phase-heads` part's public heads are
//! advanced to public with their draft ancestors (`updatephases`), its other phases moving nothing a served view
//! holds; a `phases` pushkey moves a node from its old phase to a lower one only from the phase it names, and holds
//! when the node is in the new one already (`pushphase`). The roots then change as `advanceboundary` changes them: a
//! draft root made public is removed; a changeset left draft is a root when every parent of it is public, so a new
//! draft whose parents are public is added, and so is a draft whose parent was just made public (found by a scan from
//! the lowest changeset made public, as 7.2's scan).
//!
//! **Bookmarks.** The `bookmarks` part is applied as it is (`applychanges`: a node sets, none deletes; its race is
//! `check:bookmarks`'s), a divergent name (`@` before its end) refused in 7.2's words. A `bookmarks` pushkey is a
//! compare and swap on the hex the client saw (`pushbookmark`: refused when the bookmark is neither at the old value
//! nor at the new one, or the new node is not held). A bookmark made is made from the line whose head is its node, or
//! else a line of its node's branch. Stricter than 7.2, on purpose: a node the repository does not hold, named by the
//! `bookmarks` part or a `phase-heads` part, is refused by kind, where 7.2 sets the bookmark or fails on the lookup.
//!
//! **The change in heads** (`reply:changegroup`'s `return`): the new changesets with no new child added, the old heads
//! a new changeset is a child of removed (the null node of an empty repository among them, as 7.2's `headrevsdiff`
//! counts it), the added heads that close their branch counted down ([`heads_changed`]).
//!
//! **Bounds.** Every set and walk is charged to the request's admission before it grows. A walk from a new head goes
//! down no further than the lowest head it looks for, as 7.2's branchmap walks no further than its lowest uncertain
//! head; the kept heads of the index (owed with piece 4's) make it the changed changesets' alone.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::reply::{Refused, Replied, heads_changed};
use super::verify::Arrived;
use super::{Asks, BookmarkAt, PushkeyAsk, Unbundle};
use crate::bookmarks::divergent;
use crate::phases::{Phase, PhaseHead};
use crate::read::to_u64;
use crate::serve::{Repository, Rev};
use crate::{Admission, NULL_NODE, Node, PastAdmission, changeset, node};

/// Format: the namespaces a `pushkey` part moves (`pushkey.py`'s table).
const BOOKMARKS: &[u8] = b"bookmarks";
/// Format: see [`BOOKMARKS`].
const PHASES: &[u8] = b"phases";

/// Format: 7.2's words for a divergent bookmark the `bookmarks` part names (`handlebookmark`), around the name.
const DIVERGENT_BEFORE: &[u8] = b"cannot accept divergent bookmark ";
/// Format: see [`DIVERGENT_BEFORE`].
const DIVERGENT_AFTER: &[u8] = b"!";

/// Format: the words of 7.2's failed lookup (`unknown revision '%s'`), around the node's hex.
const UNKNOWN_BEFORE: &[u8] = b"unknown revision '";
/// Format: see [`UNKNOWN_BEFORE`].
const UNKNOWN_AFTER: &[u8] = b"'";

/// Format: slates' words for a part a push may not carry yet (markers are piece 7's) and for a repository with no
/// line to make one from (never met for a repository its verb made).
const MARKERS_NOT_SERVED: &[u8] = b"obsolescence markers are not accepted by this server";
/// Format: see [`MARKERS_NOT_SERVED`].
const NO_LINE: &[u8] = b"the repository has no line to make a branch from";
/// Format: slates' words for a push past the request's admission.
const PAST_ADMISSION: &[u8] = b"push larger than this server admits";
/// Format: slates' words for a changeset whose text its reader refuses after verification (never met).
const UNREADABLE: &[u8] = b"unreadable changeset";

/// Format: the hexadecimal digits of a whole node.
const NODE_DIGITS: usize = 40;

/// Format: the draft phase's number (`phases.draft`), the phase whose roots a served view's register keeps.
pub const DRAFT: u8 = 1;

/// A line as the push read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineRead {
  /// Its named branch.
  pub branch: Vec<u8>,
  /// Its number among its branch's lines.
  pub number: u32,
  /// Its head; none for an empty line (a new repository's first).
  pub head: Option<Node>,
}

/// A bookmark as the push read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookmarkRead {
  /// Its name.
  pub name: Vec<u8>,
  /// Its node.
  pub head: Node,
}

/// What a push does to a line it read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineMove {
  /// The line moves to a new head.
  Moved {
    /// Its index among the lines read.
    line: usize,
    /// The new head.
    head: Node,
  },
  /// The line is retired: its head is merged into a new head another line took.
  Retired {
    /// Its index among the lines read.
    line: usize,
  },
}

/// A line a push makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewLine {
  /// Its named branch.
  pub branch: Vec<u8>,
  /// Its number: the lowest its branch does not use.
  pub number: u32,
  /// Its head.
  pub head: Node,
  /// The line it is made from, at the version the push read: its index among the lines read.
  pub base: usize,
}

/// What a push does to a bookmark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookmarkMove {
  /// The bookmark moves from the node the push read to another.
  Moved {
    /// Its name.
    name: Vec<u8>,
    /// Where the push read it.
    from: Node,
    /// Where it goes.
    to: Node,
    /// Whether `to` descends from `from` (a move forward), else a rewrite.
    forward: bool,
  },
  /// The bookmark is made.
  Created {
    /// Its name.
    name: Vec<u8>,
    /// Its node.
    to: Node,
    /// The line it is made from, at the version the push read: its index among the lines read.
    base: usize,
  },
  /// The bookmark is deleted.
  Deleted {
    /// Its name.
    name: Vec<u8>,
    /// Where the push read it.
    from: Node,
  },
}

/// A change of the draft phase's roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootChange {
  /// The node stops being a draft root: it is public now.
  Removed(Node),
  /// The node becomes a draft root.
  Added(Node),
}

/// What a push does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
  /// What it does to the lines read, in their order.
  pub lines: Vec<LineMove>,
  /// The lines it makes, by branch and number.
  pub created: Vec<NewLine>,
  /// What it does to bookmarks, by name.
  pub bookmarks: Vec<BookmarkMove>,
  /// The draft roots' changes: every removal, then every addition, each in node order.
  pub roots: Vec<RootChange>,
  /// The reply's parts for the parts it handled, in stream order: `reply:changegroup` and `reply:pushkey`.
  pub replied: Vec<Replied>,
  /// The first and the last changeset it added (`HG_NODE`, `HG_NODE_LAST`), if it added any.
  pub added: Option<(Node, Node)>,
  /// The branches whose lines it moves, makes or retires, sorted.
  pub touched: Vec<Vec<u8>>,
  /// Each branch it brings changesets on and the first of them in arrival order, in that order: the changeset `acl`
  /// names when it refuses the branch (A-657 part 4).
  pub firsts: Vec<(Vec<u8>, Node)>,
}

/// Why a push could not be placed; nothing of it is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unclassified {
  /// A bookmark the `bookmarks` part names whose `@` stands before its end.
  DivergentBookmark {
    /// The name.
    name: Vec<u8>,
  },
  /// A node a `phase-heads` part or the `bookmarks` part names that neither the push brought nor the repository holds
  /// (or that only a later changegroup part brings).
  UnknownNode {
    /// The node.
    node: Node,
  },
  /// A mandatory `pushkey` part that did not move its key.
  Pushkey {
    /// The part's id.
    in_reply_to: u32,
    /// What it asked.
    ask: PushkeyAsk,
  },
  /// An `obsmarkers` part: markers are piece 7's.
  Markers {
    /// The part's id.
    id: u32,
  },
  /// A line was needed to make another from, and the repository holds none.
  NoLine,
  /// A brought changeset whose text its reader refuses: never met for a verified push.
  Changeset {
    /// The changeset.
    node: Node,
    /// What the reader found.
    fault: changeset::Refusal,
  },
  /// More than the request's admission had left.
  PastAdmission(PastAdmission),
}

impl From<PastAdmission> for Unclassified {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

impl Unclassified {
  /// The error part 7.2's server answers the refusal with, in its words where it has them (module docs).
  pub fn refused(&self) -> Refused {
    let abort = |message: Vec<u8>| Refused::Abort {
      message,
      hint: None,
    };
    match self {
      Self::DivergentBookmark { name } => {
        abort([DIVERGENT_BEFORE, name.as_slice(), DIVERGENT_AFTER].concat())
      }
      Self::UnknownNode { node } => {
        let mut message = UNKNOWN_BEFORE.to_vec();
        node::write_hex(node, &mut message);
        message.extend_from_slice(UNKNOWN_AFTER);
        abort(message)
      }
      Self::Pushkey { in_reply_to, ask } => Refused::Pushkey {
        in_reply_to: *in_reply_to,
        namespace: Some(ask.namespace.clone()),
        key: Some(ask.key.clone()),
        new: Some(ask.new.clone()),
        old: Some(ask.old.clone()),
        ret: None,
      },
      Self::Markers { .. } => abort(MARKERS_NOT_SERVED.to_vec()),
      Self::NoLine => abort(NO_LINE.to_vec()),
      Self::Changeset { .. } => abort(UNREADABLE.to_vec()),
      Self::PastAdmission(_) => abort(PAST_ADMISSION.to_vec()),
    }
  }
}

/// A changeset a push names: one of the view's, or one it brought (its index among the brought ones). The view's sort
/// first, as they come first in revision order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum At {
  /// The view's revision.
  Old(Rev),
  /// The brought changeset's index.
  New(usize),
}

/// A changeset the push brought, as the walks read it.
struct Brought {
  /// Its node.
  node: Node,
  /// Its parents, each the view's or brought earlier; none for the null node or one outside the view.
  parents: [Option<At>; 2],
  /// Its branch, interned.
  branch: usize,
  /// Whether it closes its branch.
  closes: bool,
}

/// The push's changesets over the view's, and the phases as the push moves them.
struct Graph<'v, V> {
  /// The view the push read.
  view: &'v V,
  /// What the push brought, in arrival order (parents first).
  brought: Vec<Brought>,
  /// Each brought changeset's index, by node.
  by_node: BTreeMap<Node, usize>,
  /// The brought changesets' branches, interned: each name once, numbered in the order first brought.
  branches: Vec<Vec<u8>>,
  /// Each interned branch's number, by name: a push may bring a branch a changeset, so a name is found in logarithmic
  /// time, never by a walk over every branch before it.
  interned: BTreeMap<Vec<u8>, usize>,
  /// Whether the changegroup part has been taken: until then a brought changeset is not known.
  taken: bool,
  /// The view's revisions this push made public.
  published: BTreeSet<Rev>,
  /// Each brought changeset's phase.
  phases: Vec<Phase>,
}

impl<'v, V: Repository> Graph<'v, V> {
  /// The graph of `brought` over `view`, each brought changeset's branch and close read from its text.
  fn new(
    view: &'v V,
    brought: &[Arrived],
    admission: &mut Admission,
  ) -> Result<Self, Unclassified> {
    admission.charge_items::<Brought>(brought.len())?;
    admission.charge_items::<(Node, usize)>(brought.len())?;
    admission.charge_items::<Phase>(brought.len())?;
    let mut graph = Graph {
      view,
      brought: Vec::with_capacity(brought.len()),
      by_node: BTreeMap::new(),
      branches: Vec::new(),
      interned: BTreeMap::new(),
      taken: false,
      published: BTreeSet::new(),
      phases: Vec::with_capacity(brought.len()),
    };
    for (index, arrived) in brought.iter().enumerate() {
      let fault = |fault| Unclassified::Changeset {
        node: arrived.node,
        fault,
      };
      let read = changeset::decode(&arrived.text).map_err(fault)?;
      let (branch, closes) = crate::import::branch_of(&read, admission).map_err(fault)?;
      let branch = graph.intern(branch, admission)?;
      let parents = [graph.at(&arrived.p1), graph.at(&arrived.p2)];
      graph.brought.push(Brought {
        node: arrived.node,
        parents,
        branch,
        closes,
      });
      graph.by_node.insert(arrived.node, index);
      graph.phases.push(Phase::Draft);
    }
    Ok(graph)
  }

  /// `branch`'s interned number, the next one when it is new.
  fn intern(&mut self, branch: Vec<u8>, admission: &mut Admission) -> Result<usize, PastAdmission> {
    if let Some(at) = self.interned.get(&branch) {
      return Ok(*at);
    }
    let bytes = to_u64(branch.len());
    admission.charge(bytes)?;
    admission.charge_items::<Vec<u8>>(1)?;
    // The copy of the name the index keys it by.
    admission.charge(bytes)?;
    admission.charge_items::<(Vec<u8>, usize)>(1)?;
    let at = self.branches.len();
    self.interned.insert(branch.clone(), at);
    self.branches.push(branch);
    Ok(at)
  }

  /// Where `node` stands: brought, or the view's; none for the null node or one outside both.
  fn at(&self, node: &Node) -> Option<At> {
    if *node == NULL_NODE {
      return None;
    }
    if let Some(index) = self.by_node.get(node) {
      return Some(At::New(*index));
    }
    self.view.rev(node).map(At::Old)
  }

  /// Where `node` stands as a part names it: a brought changeset only once the changegroup part was taken.
  fn known(&self, node: &Node) -> Option<At> {
    self
      .at(node)
      .filter(|at| self.taken || matches!(at, At::Old(_)))
  }

  /// The node of a changeset.
  fn node(&self, at: At) -> Option<Node> {
    match at {
      At::Old(rev) => self.view.node(rev),
      At::New(index) => self.brought.get(index).map(|brought| brought.node),
    }
  }

  /// A changeset's parents.
  fn parents(&self, at: At) -> [Option<At>; 2] {
    match at {
      At::Old(rev) => self.view.parents(rev).map(|parent| parent.map(At::Old)),
      At::New(index) => self
        .brought
        .get(index)
        .map_or([None, None], |brought| brought.parents),
    }
  }

  /// A changeset's branch.
  fn branch(&self, at: At) -> &[u8] {
    match at {
      At::Old(rev) => self.view.branch(rev),
      At::New(index) => self
        .brought
        .get(index)
        .and_then(|brought| self.branches.get(brought.branch))
        .map(Vec::as_slice)
        .unwrap_or_default(),
    }
  }

  /// A changeset's phase as the push has moved it so far; the null node and a node outside the view are public.
  fn phase(&self, at: Option<At>) -> Phase {
    match at {
      None => Phase::Public,
      Some(At::Old(rev)) if self.published.contains(&rev) => Phase::Public,
      Some(At::Old(rev)) => self.view.phase(rev),
      Some(At::New(index)) => self.phases.get(index).copied().unwrap_or(Phase::Public),
    }
  }

  /// Makes `seed` and every draft ancestor of it public (`advanceboundary` to public).
  fn publish(&mut self, seed: At, admission: &mut Admission) -> Result<(), PastAdmission> {
    let mut stack = vec![seed];
    while let Some(at) = stack.pop() {
      if self.phase(Some(at)) != Phase::Draft {
        continue;
      }
      match at {
        At::Old(rev) => {
          admission.charge_items::<Rev>(1)?;
          self.published.insert(rev);
        }
        At::New(index) => {
          if let Some(phase) = self.phases.get_mut(index) {
            *phase = Phase::Public;
          }
        }
      }
      let parents = self.parents(at);
      admission.charge_items::<At>(parents.len())?;
      stack.extend(parents.into_iter().flatten());
    }
    Ok(())
  }

  /// The changegroup part taken: every brought changeset draft, or on a publishing repository public with every draft
  /// ancestor of what the changegroup carried (`apply`).
  fn take_changegroup(
    &mut self,
    held: &[Node],
    publishing: bool,
    admission: &mut Admission,
  ) -> Result<(), PastAdmission> {
    self.taken = true;
    let phase = if publishing {
      Phase::Public
    } else {
      Phase::Draft
    };
    for slot in &mut self.phases {
      *slot = phase;
    }
    if !publishing {
      return Ok(());
    }
    let parents: Vec<At> = self
      .brought
      .iter()
      .flat_map(|brought| brought.parents.into_iter().flatten())
      .collect();
    let held: Vec<At> = held.iter().filter_map(|node| self.at(node)).collect();
    for seed in parents.into_iter().chain(held) {
      self.publish(seed, admission)?;
    }
    Ok(())
  }

  /// Whether `ancestor` is `at` or an ancestor of it.
  fn descends(
    &self,
    at: At,
    ancestor: At,
    admission: &mut Admission,
  ) -> Result<bool, PastAdmission> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![at];
    while let Some(next) = stack.pop() {
      if next == ancestor {
        return Ok(true);
      }
      // Nothing below the ancestor leads to it: a parent comes before its child, the view's before the brought.
      if next < ancestor || !seen.insert(next) {
        continue;
      }
      let parents = self.parents(next);
      admission.charge_items::<At>(parents.len().saturating_add(1))?;
      stack.extend(parents.into_iter().flatten());
    }
    Ok(false)
  }

  /// The view's revisions among `targets` that `at` is or descends from, walking no lower than the lowest target.
  fn reached(
    &self,
    at: At,
    targets: &BTreeSet<Rev>,
    admission: &mut Admission,
  ) -> Result<BTreeSet<Rev>, PastAdmission> {
    let mut found = BTreeSet::new();
    let Some(floor) = targets.first().copied() else {
      return Ok(found);
    };
    let mut seen = BTreeSet::new();
    let mut stack = vec![at];
    while let Some(next) = stack.pop() {
      if let At::Old(rev) = next {
        if rev < floor {
          continue;
        }
        if targets.contains(&rev) {
          found.insert(rev);
        }
      }
      if !seen.insert(next) {
        continue;
      }
      let parents = self.parents(next);
      admission.charge_items::<At>(parents.len().saturating_add(1))?;
      stack.extend(parents.into_iter().flatten());
    }
    Ok(found)
  }
}

/// Places a verified push (module docs): `brought` the changesets it added in arrival order and `held` the changesets
/// its changegroup carried that the repository held; `lines` every line the push read and `marks` the bookmarks it
/// names; `view` the served view it read.
pub fn classify(
  unbundle: &Unbundle,
  (brought, held): (&[Arrived], &[Node]),
  view: &impl Repository,
  (lines, marks): (&[LineRead], &[BookmarkRead]),
  publishing: bool,
  admission: &mut Admission,
) -> Result<Plan, Unclassified> {
  let mut graph = Graph::new(view, brought, admission)?;
  let mut plan = Plan::default();
  let mut bookmarks = Marks::new(marks);
  for taken in &unbundle.parts {
    match &taken.asks {
      Asks::Changegroup(_) => {
        graph.take_changegroup(held, publishing, admission)?;
        plan.replied.push(Replied::Changegroup {
          in_reply_to: taken.id,
          heads: heads_change(&graph, admission)?,
        });
      }
      Asks::PhaseHeads(entries) => phase_heads(&mut graph, entries, admission)?,
      Asks::Bookmarks(entries) => bookmarks.apply(&graph, entries, admission)?,
      Asks::Pushkey(ask) => {
        let moved = match ask.namespace.as_slice() {
          BOOKMARKS => bookmarks.pushkey(&graph, ask, admission)?,
          PHASES => pushphase(&mut graph, ask, admission)?,
          _ => false,
        };
        if !moved && taken.mandatory {
          return Err(Unclassified::Pushkey {
            in_reply_to: taken.id,
            ask: ask.clone(),
          });
        }
        plan.replied.push(Replied::Pushkey {
          in_reply_to: taken.id,
          moved,
        });
      }
      Asks::Obsmarkers(_) => return Err(Unclassified::Markers { id: taken.id }),
      _ => {}
    }
  }
  let (moves, created, touched) = place_lines(&graph, lines, admission)?;
  plan.lines = moves;
  plan.created = created;
  plan.touched = touched;
  plan.bookmarks = bookmarks.moves(&graph, lines, admission)?;
  plan.roots = roots(&graph, admission)?;
  plan.added = graph
    .brought
    .first()
    .zip(graph.brought.last())
    .map(|(first, last)| (first.node, last.node));
  admission.charge_items::<bool>(graph.branches.len())?;
  let mut seen = vec![false; graph.branches.len()];
  for brought in &graph.brought {
    if let Some(first) = seen.get_mut(brought.branch).filter(|seen| !**seen) {
      *first = true;
      let branch = graph
        .branches
        .get(brought.branch)
        .cloned()
        .unwrap_or_default();
      admission.charge(to_u64(branch.len()))?;
      admission.charge_items::<(Vec<u8>, Node)>(1)?;
      plan.firsts.push((branch, brought.node));
    }
  }
  Ok(plan)
}

/// `reply:changegroup`'s `return` for what the push brought (module docs).
fn heads_change<V: Repository>(
  graph: &Graph<'_, V>,
  admission: &mut Admission,
) -> Result<i64, PastAdmission> {
  let count = graph.brought.len();
  admission.charge_items::<bool>(count)?;
  let mut has_child = vec![false; count];
  let mut candidates = BTreeSet::new();
  for brought in &graph.brought {
    for parent in brought.parents.into_iter().flatten() {
      match parent {
        At::New(index) => {
          if let Some(slot) = has_child.get_mut(index) {
            *slot = true;
          }
        }
        At::Old(rev) => {
          admission.charge_items::<Rev>(1)?;
          candidates.insert(rev);
        }
      }
    }
  }
  let added_heads = graph
    .brought
    .iter()
    .zip(&has_child)
    .filter(|(_, child)| !**child);
  let added = to_u64(added_heads.clone().count());
  let closing = to_u64(added_heads.filter(|(brought, _)| brought.closes).count());
  if let Some(first) = candidates.first().copied() {
    for rev in first.saturating_add(1)..graph.view.count() {
      for parent in graph.view.parents(rev).into_iter().flatten() {
        candidates.remove(&parent);
      }
    }
  }
  let empty = u64::from(graph.view.count() == 0 && count > 0);
  let removed = to_u64(candidates.len()).saturating_add(empty);
  Ok(heads_changed(removed, added, closing))
}

/// A `phase-heads` part: its public heads advanced to public with their draft ancestors (`updatephases`).
fn phase_heads<V: Repository>(
  graph: &mut Graph<'_, V>,
  entries: &[PhaseHead],
  admission: &mut Admission,
) -> Result<(), Unclassified> {
  for entry in entries {
    let at = graph
      .known(&entry.node)
      .ok_or(Unclassified::UnknownNode { node: entry.node })?;
    if entry.phase == Phase::Public {
      graph.publish(at, admission)?;
    }
  }
  Ok(())
}

/// A `phases` pushkey (`pushphase`): whether it holds.
fn pushphase<V: Repository>(
  graph: &mut Graph<'_, V>,
  ask: &PushkeyAsk,
  admission: &mut Admission,
) -> Result<bool, PastAdmission> {
  let number = |value: &[u8]| {
    core::str::from_utf8(value)
      .ok()
      .and_then(|text| text.parse::<i64>().ok())
      .map(i64::unsigned_abs)
  };
  let (Some(node), Some(old), Some(new)) = (hex_node(&ask.key), number(&ask.old), number(&ask.new))
  else {
    return Ok(false);
  };
  let Some(at) = graph.known(&node) else {
    return Ok(false);
  };
  let current = u64::from(graph.phase(Some(at)).number().unsigned_abs());
  if current == old && new < old {
    if new == u64::from(Phase::Public.number().unsigned_abs()) {
      graph.publish(at, admission)?;
    }
    return Ok(true);
  }
  Ok(current == new)
}

/// A node from its forty hexadecimal digits.
fn hex_node(hex: &[u8]) -> Option<Node> {
  if hex.len() != NODE_DIGITS {
    return None;
  }
  node::from_hex(hex)
}

/// The bookmarks a push names: each as read, and as the push has set it so far.
struct Marks {
  /// Each name: where the push read it, and where it stands now.
  marks: BTreeMap<Vec<u8>, (Option<Node>, Option<Node>)>,
  /// The bookmarks as read.
  read: BTreeMap<Vec<u8>, Node>,
}

impl Marks {
  /// The bookmarks as read.
  fn new(read: &[BookmarkRead]) -> Self {
    Marks {
      marks: BTreeMap::new(),
      read: read
        .iter()
        .map(|mark| (mark.name.clone(), mark.head))
        .collect(),
    }
  }

  /// `name`'s place now, read on first use.
  fn slot(
    &mut self,
    name: &[u8],
    admission: &mut Admission,
  ) -> Result<&mut (Option<Node>, Option<Node>), PastAdmission> {
    match self.marks.entry(name.to_vec()) {
      Entry::Occupied(slot) => Ok(slot.into_mut()),
      Entry::Vacant(slot) => {
        admission.charge(to_u64(name.len()))?;
        admission.charge_items::<(Vec<u8>, (Option<Node>, Option<Node>))>(1)?;
        let read = self.read.get(name).copied();
        Ok(slot.insert((read, read)))
      }
    }
  }

  /// The `bookmarks` part applied as it is (`applychanges`).
  fn apply<V: Repository>(
    &mut self,
    graph: &Graph<'_, V>,
    entries: &[BookmarkAt],
    admission: &mut Admission,
  ) -> Result<(), Unclassified> {
    for entry in entries {
      if divergent(&entry.name) {
        return Err(Unclassified::DivergentBookmark {
          name: entry.name.clone(),
        });
      }
      if let Some(node) = entry.node
        && graph.known(&node).is_none()
      {
        return Err(Unclassified::UnknownNode { node });
      }
      self.slot(&entry.name, admission)?.1 = entry.node;
    }
    Ok(())
  }

  /// A `bookmarks` pushkey (`pushbookmark`): whether it holds.
  fn pushkey<V: Repository>(
    &mut self,
    graph: &Graph<'_, V>,
    ask: &PushkeyAsk,
    admission: &mut Admission,
  ) -> Result<bool, PastAdmission> {
    if divergent(&ask.key) {
      return Ok(false);
    }
    let now = self.slot(&ask.key, admission)?.1;
    let mut existing = Vec::with_capacity(NODE_DIGITS);
    if let Some(node) = now {
      node::write_hex(&node, &mut existing);
    }
    if existing != ask.old && existing != ask.new {
      return Ok(false);
    }
    let to = if ask.new.is_empty() {
      None
    } else {
      match hex_node(&ask.new).filter(|node| graph.known(node).is_some()) {
        Some(node) => Some(node),
        None => return Ok(false),
      }
    };
    self.slot(&ask.key, admission)?.1 = to;
    Ok(true)
  }

  /// Every bookmark the push changed, by name.
  fn moves<V: Repository>(
    &self,
    graph: &Graph<'_, V>,
    lines: &[LineRead],
    admission: &mut Admission,
  ) -> Result<Vec<BookmarkMove>, Unclassified> {
    let mut moves = Vec::new();
    for (name, (read, now)) in &self.marks {
      admission.charge_items::<BookmarkMove>(1)?;
      match (*read, *now) {
        (Some(from), Some(to)) if from != to => {
          let forward = match (graph.at(&from), graph.at(&to)) {
            (Some(from), Some(to)) => graph.descends(to, from, admission)?,
            _ => false,
          };
          moves.push(BookmarkMove::Moved {
            name: name.clone(),
            from,
            to,
            forward,
          });
        }
        (None, Some(to)) => moves.push(BookmarkMove::Created {
          name: name.clone(),
          to,
          base: mark_base(graph, lines, &to)?,
        }),
        (Some(from), None) => moves.push(BookmarkMove::Deleted {
          name: name.clone(),
          from,
        }),
        _ => {}
      }
    }
    Ok(moves)
  }
}

/// The line a bookmark at `to` is made from: the line whose head is `to`, else the first line of `to`'s branch, else
/// the first line.
fn mark_base<V: Repository>(
  graph: &Graph<'_, V>,
  lines: &[LineRead],
  to: &Node,
) -> Result<usize, Unclassified> {
  if let Some(line) = lines.iter().position(|line| line.head == Some(*to)) {
    return Ok(line);
  }
  let branch = graph.at(to).map(|at| graph.branch(at));
  first_line(lines, branch)
}

/// The first line of `branch` by number, else the first line of all by branch and number.
fn first_line(lines: &[LineRead], branch: Option<&[u8]>) -> Result<usize, Unclassified> {
  let order = |line: &&LineRead| (line.branch.clone(), line.number);
  let of_branch = lines
    .iter()
    .enumerate()
    .filter(|(_, line)| Some(line.branch.as_slice()) == branch)
    .min_by_key(|(_, line)| line.number);
  of_branch
    .or_else(|| lines.iter().enumerate().min_by_key(|(_, line)| order(line)))
    .map(|(index, _)| index)
    .ok_or(Unclassified::NoLine)
}

/// The lines' moves, the lines made and the branches touched (module docs).
type Placed = (Vec<LineMove>, Vec<NewLine>, Vec<Vec<u8>>);

/// Places the brought changesets on the lines (module docs).
fn place_lines<V: Repository>(
  graph: &Graph<'_, V>,
  lines: &[LineRead],
  admission: &mut Admission,
) -> Result<Placed, Unclassified> {
  let heads = new_heads(graph, admission)?;
  let mut moves = Vec::new();
  let mut created = Vec::new();
  let mut touched = Vec::new();
  for (branch, heads) in heads {
    let name = graph.branches.get(branch).cloned().unwrap_or_default();
    let (moved, made) = place_branch(graph, (&name, &heads), lines, admission)?;
    if moved.is_empty() && made.is_empty() {
      continue;
    }
    moves.extend(moved);
    created.extend(made);
    touched.push(name);
  }
  moves.sort_by_key(|placed| match placed {
    LineMove::Moved { line, .. } | LineMove::Retired { line } => *line,
  });
  touched.sort();
  Ok((moves, created, touched))
}

/// Each branch's new heads among the brought changesets, in arrival order: those no brought descendant of which is on
/// their branch.
fn new_heads<V: Repository>(
  graph: &Graph<'_, V>,
  admission: &mut Admission,
) -> Result<BTreeMap<usize, Vec<usize>>, PastAdmission> {
  let count = graph.brought.len();
  admission.charge_items::<Vec<usize>>(count)?;
  admission.charge_items::<BTreeSet<usize>>(count)?;
  let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
  for (index, brought) in graph.brought.iter().enumerate() {
    for parent in brought.parents.into_iter().flatten() {
      if let At::New(parent) = parent
        && let Some(slot) = children.get_mut(parent)
      {
        admission.charge_items::<usize>(1)?;
        slot.push(index);
      }
    }
  }
  // Each brought changeset's branches among it and its brought descendants, children before parents.
  let mut below: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); count];
  let mut heads: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
  for index in (0..count).rev() {
    let own = graph.brought.get(index).map_or(0, |brought| brought.branch);
    let mut set = BTreeSet::from([own]);
    let mut continued = false;
    for child in children.get(index).into_iter().flatten() {
      for branch in below.get(*child).into_iter().flatten() {
        continued |= *branch == own;
        if set.insert(*branch) {
          admission.charge_items::<usize>(1)?;
        }
      }
    }
    if !continued {
      admission.charge_items::<usize>(1)?;
      heads.entry(own).or_default().push(index);
    }
    if let Some(slot) = below.get_mut(index) {
      *slot = set;
    }
  }
  for heads in heads.values_mut() {
    heads.reverse();
  }
  Ok(heads)
}

/// One branch's placement: the moves and retirements of its lines, and the lines made.
fn place_branch<V: Repository>(
  graph: &Graph<'_, V>,
  (branch, heads): (&[u8], &[usize]),
  lines: &[LineRead],
  admission: &mut Admission,
) -> Result<(Vec<LineMove>, Vec<NewLine>), Unclassified> {
  let mut own: Vec<usize> = (0..lines.len())
    .filter(|line| lines.get(*line).is_some_and(|read| read.branch == branch))
    .collect();
  own.sort_by_key(|line| lines.get(*line).map(|read| read.number));
  let edges = edges(graph, heads, (&own, lines), admission)?;
  let matched = matching(&edges, admission)?;
  let mut moves = Vec::new();
  let mut superseded: BTreeSet<usize> = BTreeSet::new();
  for lines_of in &edges {
    superseded.extend(
      lines_of
        .iter()
        .filter(|line| lines.get(**line).is_some_and(|read| read.head.is_some())),
    );
  }
  let taken: BTreeSet<usize> = matched.iter().flatten().copied().collect();
  for (head, line) in heads.iter().zip(&matched) {
    if let (Some(line), Some(node)) = (line, graph.node(At::New(*head))) {
      moves.push(LineMove::Moved {
        line: *line,
        head: node,
      });
    }
  }
  moves.extend(
    superseded
      .difference(&taken)
      .map(|line| LineMove::Retired { line: *line }),
  );
  let mut used: BTreeSet<u32> = own
    .iter()
    .filter_map(|line| lines.get(*line).map(|read| read.number))
    .collect();
  let mut made = Vec::new();
  for ((head, line), lines_of) in heads.iter().zip(&matched).zip(&edges) {
    if line.is_some() {
      continue;
    }
    let Some(node) = graph.node(At::New(*head)) else {
      continue;
    };
    let number = (0..=u32::MAX)
      .find(|number| !used.contains(number))
      .unwrap_or(u32::MAX);
    used.insert(number);
    admission.charge_items::<NewLine>(1)?;
    made.push(NewLine {
      branch: branch.to_vec(),
      number,
      head: node,
      base: line_base(graph, (*head, lines_of), lines)?,
    });
  }
  Ok((moves, made))
}

/// For each new head, the lines of its branch it descends from, in their numbers' order: an empty line always.
fn edges<V: Repository>(
  graph: &Graph<'_, V>,
  heads: &[usize],
  (own, lines): (&[usize], &[LineRead]),
  admission: &mut Admission,
) -> Result<Vec<Vec<usize>>, PastAdmission> {
  admission.charge_items::<Rev>(own.len())?;
  let revs: BTreeSet<Rev> = own
    .iter()
    .filter_map(|line| lines.get(*line).and_then(|read| read.head))
    .filter_map(|head| graph.view.rev(&head))
    .collect();
  let mut edges = Vec::with_capacity(heads.len());
  for head in heads {
    let reached = graph.reached(At::New(*head), &revs, admission)?;
    let lines_of: Vec<usize> = own
      .iter()
      .copied()
      .filter(|line| {
        lines.get(*line).is_some_and(|read| match read.head {
          None => true,
          Some(node) => graph
            .view
            .rev(&node)
            .is_some_and(|rev| reached.contains(&rev)),
        })
      })
      .collect();
    admission.charge_items::<usize>(lines_of.len())?;
    edges.push(lines_of);
  }
  Ok(edges)
}

/// A maximum matching of new heads to lines (augmenting paths found breadth first, the heads in order): each head's
/// line, or none.
fn matching(
  edges: &[Vec<usize>],
  admission: &mut Admission,
) -> Result<Vec<Option<usize>>, PastAdmission> {
  admission.charge_items::<Option<usize>>(edges.len())?;
  let mut line_of: Vec<Option<usize>> = vec![None; edges.len()];
  let mut head_of: BTreeMap<usize, usize> = BTreeMap::new();
  for start in 0..edges.len() {
    let mut queue = VecDeque::from([start]);
    let mut came: BTreeMap<usize, usize> = BTreeMap::new();
    let mut free = None;
    while let Some(head) = queue.pop_front() {
      for line in edges.get(head).into_iter().flatten() {
        if came.contains_key(line) {
          continue;
        }
        admission.charge_items::<(usize, usize)>(1)?;
        came.insert(*line, head);
        match head_of.get(line) {
          Some(next) => queue.push_back(*next),
          None => {
            free = Some(*line);
            break;
          }
        }
      }
      if free.is_some() {
        break;
      }
    }
    // Augments along the path that found a free line, back to `start`.
    let mut line = free;
    while let Some(at) = line {
      let Some(head) = came.get(&at).copied() else {
        break;
      };
      let previous = line_of.get(head).copied().flatten();
      if let Some(slot) = line_of.get_mut(head) {
        *slot = Some(at);
      }
      head_of.insert(at, head);
      line = previous;
    }
  }
  Ok(line_of)
}

/// The line a new line at brought changeset `head` is made from (module docs).
fn line_base<V: Repository>(
  graph: &Graph<'_, V>,
  (head, lines_of): (usize, &[usize]),
  lines: &[LineRead],
) -> Result<usize, Unclassified> {
  let with_head = lines_of
    .iter()
    .copied()
    .find(|line| lines.get(*line).is_some_and(|read| read.head.is_some()));
  if let Some(line) = with_head.or_else(|| lines_of.first().copied()) {
    return Ok(line);
  }
  // The first changeset the repository held under the head's first parents.
  let mut at = At::New(head);
  let held = loop {
    let [first, _] = graph.parents(at);
    match first {
      Some(At::New(parent)) => at = At::New(parent),
      Some(At::Old(rev)) => break Some(rev),
      None => break None,
    }
  };
  let Some(rev) = held else {
    return first_line(lines, None);
  };
  let node = graph.view.node(rev);
  if let Some(line) = lines
    .iter()
    .position(|line| line.head.is_some() && line.head == node)
  {
    return Ok(line);
  }
  first_line(lines, Some(graph.view.branch(rev)))
}

/// The draft roots' changes (module docs).
fn roots<V: Repository>(
  graph: &Graph<'_, V>,
  admission: &mut Admission,
) -> Result<Vec<RootChange>, PastAdmission> {
  let view = graph.view;
  let old_public =
    |parent: Option<Rev>| parent.is_none_or(|parent| view.phase(parent) == Phase::Public);
  let mut removed = BTreeSet::new();
  for rev in &graph.published {
    if view.parents(*rev).into_iter().all(old_public)
      && let Some(node) = view.node(*rev)
    {
      admission.charge_items::<Node>(1)?;
      removed.insert(node);
    }
  }
  let mut added = BTreeSet::new();
  let root_now = |at: At| {
    graph.phase(Some(at)) == Phase::Draft
      && graph
        .parents(at)
        .into_iter()
        .all(|parent| graph.phase(parent) == Phase::Public)
  };
  for index in 0..graph.brought.len() {
    if root_now(At::New(index))
      && let Some(node) = graph.node(At::New(index))
    {
      admission.charge_items::<Node>(1)?;
      added.insert(node);
    }
  }
  if let Some(first) = graph.published.first().copied() {
    for rev in first.saturating_add(1)..view.count() {
      let child_of_published = view
        .parents(rev)
        .into_iter()
        .flatten()
        .any(|parent| graph.published.contains(&parent));
      if child_of_published
        && root_now(At::Old(rev))
        && let Some(node) = view.node(rev)
      {
        admission.charge_items::<Node>(1)?;
        added.insert(node);
      }
    }
  }
  Ok(
    removed
      .into_iter()
      .map(RootChange::Removed)
      .chain(added.into_iter().map(RootChange::Added))
      .collect(),
  )
}

#[cfg(test)]
#[path = "classify_tests.rs"]
mod tests;
