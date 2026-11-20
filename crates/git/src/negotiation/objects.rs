//! The objects a pack carries one by one (A-615 part 2; git's `list-objects.c` `traverse_commit_list_filtered` and
//! `mark_edges_uninteresting`, `list-objects-filter.c`, and `builtin/pack-objects.c` `add_tag_chain`, without bitmaps;
//! git v2.50.1).
//!
//! 1. **Commits.** The walk paints the wants answered by no run (and the parents of commits a cut made whole) as wanted,
//!    and the common haves, peeled to commits (and the commits a cut made whole, and the wants a run answers, whose
//!    histories the runs carry) as had. The commits to send are those wanted and not had.
//! 2. **Left out.** The trees of the edge commits, each had parent of a commit sent, and the trees and blobs a have
//!    names that is no commit: nothing in them is sent. For a thin pack in a shallow exchange (the client is shallow,
//!    or the cut is new), git's server runs `pack-objects --objects-edge-aggressive`, which leaves out the tree of
//!    every had commit on its command line (`mark_edges_uninteresting`): each have `do_got_oid` lists (`have_obj`: a
//!    commit only when no have before it named it or named it as a parent) and each commit a cut made whole
//!    (`send_unshallow`'s `extra_edge_obj`); so does this. Without a thin pack only the edges are left out. (A-640
//!    part 3 found the aggressive rule missing: git's own `--deepen` fetch, served, sent the shallow tip's blobs again;
//!    git's stateless `deepen 2` without `thin-pack` sent them, which held the rule to thin packs alone.)
//! 3. **Trees and blobs.** Each sent commit's root tree is opened at depth 0, its entries at depth 1, and so on; a tree
//!    or blob is sent when nothing left out holds it and the filter admits it at the least depth it is met at. A tree met
//!    again shallower is opened again, as git's `tree:` filter reopens it; under any other filter each tree opens once.
//!    Under a `sparse:oid` filter, served alone (A-640 part 4), the walk is git's `filter_sparse` [C:
//!    `list-objects-filter.c`, `list-objects.c` `process_tree` and `process_blob`]: each tree it reaches is sent, and a
//!    blob when the path it is met at is matched, a path the patterns leave undecided taking the verdict of the tree it
//!    is in, and the root's base being not matched. A tree no blob below which was left out is done and never walked
//!    again; any other is walked again wherever it is met, so a blob left out at one path is sent when another path to
//!    it is matched. What is sent is what some path decides, whatever order the trees are met in.
//! 4. **The wants themselves**, and every tag a want's chain of tags passes through, are sent whatever the filter; a
//!    wanted tree's entries are met at depth 0, as git meets the entries of a tree a client named.
//! 5. **`include-tag`**: each annotated tag under `refs/tags/` whose chain ends at an object the pack carries is sent,
//!    with every tag of its chain, whatever the filter.

use std::collections::{BTreeMap, BTreeSet};

use super::paint::{HAVE, Paint, WANT};
use super::ready::peel;
use super::shallow::Cut;
use super::sparse::Verdict;
use super::{Counters, Filter, Graph, Held, Patterns, Peak, Refusal, Request, Run, charge};
use crate::Admission;
use crate::object_name::{Kind, ObjectId};
use crate::positions::Positions;

/// Format: the byte between a path's segments.
const SLASH: u8 = b'/';

/// The sides of a pack: what the request wants and has, and the cut its shallow lines make.
pub(super) struct Sides<'s, N> {
  /// The wants.
  pub(super) wants: &'s [N],
  /// The wants a run answers.
  pub(super) one_round: &'s [N],
  /// The common haves.
  pub(super) common: &'s [N],
  /// The cut.
  pub(super) cut: &'s Cut<N>,
}

/// The objects to send one by one, each once, in handle order.
pub(super) fn select<S: Graph>(
  graph: &S,
  request: &Request<'_>,
  sides: &Sides<'_, S::Node>,
  runs: &[Run<S::Green>],
  admission: &mut Admission,
  counters: &mut Counters,
) -> Result<Positions, Refusal> {
  let sent = commits(graph, sides, admission, counters)?;
  let left_out = left_out(graph, request, sides, &sent, admission)?;
  let depth_decides = request.filter.is_some_and(|filter| !filter.is_per_object());
  let mut chosen = Chosen {
    filter: request.filter,
    sparse: sparse_patterns(request),
    left_out: &left_out,
    objects: Held::default(),
    opened: if depth_decides {
      Opened::AtDepth(BTreeMap::new())
    } else {
      Opened::Once(Held::default())
    },
    done: Held::default(),
    stack: Peak::default(),
  };
  for commit in sent
    .iter()
    .filter_map(|position| S::Node::try_from(position).ok())
  {
    if chosen.admits(Kind::Commit, 0, 0) {
      chosen.take(commit, admission)?;
    }
    if let Some(row) = graph.commit(commit) {
      chosen.root(graph, row.tree, admission, counters)?;
    }
  }
  for want in sides
    .wants
    .iter()
    .filter(|want| !sides.one_round.contains(want))
  {
    chosen.want(graph, *want, admission, counters)?;
  }
  if request.include_tag {
    let carried = carried_by_runs(graph, runs, request.filter, admission)?;
    chosen.tags(graph, &carried, admission)?;
  }
  Ok(chosen.objects.into_positions())
}

/// The commits to send: wanted and not had.
fn commits<S: Graph>(
  graph: &S,
  sides: &Sides<'_, S::Node>,
  admission: &mut Admission,
  counters: &mut Counters,
) -> Result<Positions, Refusal> {
  let mut paint = Paint::new();
  let walked = sides
    .wants
    .iter()
    .filter(|want| !sides.one_round.contains(want))
    .chain(&sides.cut.wants);
  for want in walked.filter_map(|node| commit_of(graph, *node)) {
    paint.paint(graph, want, WANT, admission)?;
  }
  let had = sides
    .common
    .iter()
    .chain(&sides.cut.haves)
    .chain(sides.one_round);
  for have in had.filter_map(|node| commit_of(graph, *node)) {
    paint.paint(graph, have, HAVE, admission)?;
  }
  paint.run(graph, &sides.cut.grafts, |_| false, admission, counters)?;
  let sent = paint.chosen();
  admission
    .charge_items::<(u64, u64)>(sent.runs())
    .map_err(Refusal::PastAdmission)?;
  Ok(sent)
}

/// The commit `node` names through its tags, if it names one.
fn commit_of<S: Graph>(graph: &S, node: S::Node) -> Option<S::Node> {
  peel(graph, node).filter(|peeled| graph.commit(*peeled).is_some())
}

/// What the client holds of trees and blobs: everything in the edge commits' trees and in the haves that are no
/// commits; and, for a thin pack in a shallow exchange, everything in each had commit's tree on git's command line.
fn left_out<S: Graph>(
  graph: &S,
  request: &Request<'_>,
  sides: &Sides<'_, S::Node>,
  sent: &Positions,
  admission: &mut Admission,
) -> Result<Held, Refusal> {
  let mut held = Held::default();
  let mut stack = Peak::default();
  for commit in sent
    .iter()
    .filter_map(|position| S::Node::try_from(position).ok())
    .filter(|commit| !sides.cut.grafts.contains(commit))
  {
    for parent in graph
      .parents(commit)
      .filter(|parent| !sent.contains((*parent).into()))
    {
      if let Some(row) = graph.commit(parent) {
        mark(graph, row.tree, &mut held, &mut stack, admission)?;
      }
    }
  }
  left_out_aggressively(graph, request, sides, &mut held, &mut stack, admission)?;
  for have in sides.common.iter().filter_map(|node| peel(graph, *node)) {
    if graph.commit(have).is_none() {
      mark(graph, have, &mut held, &mut stack, admission)?;
    }
  }
  Ok(held)
}

/// For a thin pack in a shallow exchange, adds to `held` everything in the tree of each had commit git's command line
/// names: each have it lists, and each commit a cut made whole (`--objects-edge-aggressive`).
fn left_out_aggressively<S: Graph>(
  graph: &S,
  request: &Request<'_>,
  sides: &Sides<'_, S::Node>,
  held: &mut Held,
  stack: &mut Peak,
  admission: &mut Admission,
) -> Result<(), Refusal> {
  if !request.thin || sides.cut.grafts.is_empty() {
    return Ok(());
  }
  let listed = listed_haves(graph, request.haves, admission)?;
  for had in listed.iter().chain(&sides.cut.haves) {
    if let Some(row) = graph.commit(*had) {
      mark(graph, row.tree, held, stack, admission)?;
    }
  }
  Ok(())
}

/// The haves git lists for `pack-objects` (`do_got_oid`'s `have_obj`), those the store holds: in the order sent, a
/// commit only when no have before it named it or named it as a parent, any other object each time.
fn listed_haves<S: Graph>(
  graph: &S,
  haves: &[ObjectId],
  admission: &mut Admission,
) -> Result<Vec<S::Node>, Refusal> {
  let mut marked = BTreeSet::new();
  let mut listed = Vec::new();
  for node in haves.iter().filter_map(|id| graph.node(id)) {
    let counts = if graph.commit(node).is_some() {
      let new = marked.insert(node);
      for parent in graph.parents(node) {
        charge::<S::Node>(admission)?;
        marked.insert(parent);
      }
      new
    } else {
      true
    };
    if counts {
      charge::<(S::Node, S::Node)>(admission)?;
      listed.push(node);
    }
  }
  Ok(listed)
}

/// Adds `root` and everything below it to `held`, without recursion, a tree already held skipped whole; the stack
/// charged at its deepest (`peak`).
fn mark<S: Graph>(
  graph: &S,
  root: S::Node,
  held: &mut Held,
  peak: &mut Peak,
  admission: &mut Admission,
) -> Result<(), Refusal> {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if !held.insert(node, admission)? {
      continue;
    }
    if graph.kind(node) == Kind::Tree {
      stack.extend(graph.entries(node));
      peak.reach::<S::Node>(stack.len(), admission)?;
    }
  }
  Ok(())
}

/// The objects the runs carry that the filter admits, kept only for `include-tag` to ask of.
fn carried_by_runs<S: Graph>(
  graph: &S,
  runs: &[Run<S::Green>],
  filter: Option<&Filter>,
  admission: &mut Admission,
) -> Result<Held, Refusal> {
  let mut carried = Held::default();
  for run in runs {
    for version in run.after.saturating_add(1)..=run.through {
      for node in graph.brought(run.green, version) {
        let kind = graph.kind(node);
        let size = if kind == Kind::Blob {
          graph.size(node)
        } else {
          0
        };
        if filter.is_none_or(|filter| filter.admits(kind, size, 0)) {
          carried.insert(node, admission)?;
        }
      }
    }
  }
  Ok(carried)
}

/// The objects chosen so far, and what the choice reads.
struct Chosen<'c, N> {
  /// The request's filter.
  filter: Option<&'c Filter>,
  /// What the client holds of trees and blobs.
  left_out: &'c Held,
  /// The objects chosen.
  objects: Held,
  /// Each tree opened (and, under a `tree:` filter, the depth its entries were met at, the least so far).
  opened: Opened<N>,
  /// The patterns of a `sparse:oid` filter, which decide by path.
  sparse: Option<&'c Patterns>,
  /// Under a `sparse:oid` filter, each tree no blob below which was left out: never walked again.
  done: Held,
  /// The tree walk's stack, charged at its deepest.
  stack: Peak,
}

/// The trees a walk opened: as runs where each opens once (every filter but `tree:`), with the depth each was last met
/// at where that depth decides what is sent below it (a `tree:` filter, which reopens a tree met again shallower).
enum Opened<N> {
  /// Each opened once.
  Once(Held),
  /// Each with the least depth its entries were met at.
  AtDepth(BTreeMap<N, u64>),
}

/// One `sparse:oid` walk from a root tree: the path of the tree being walked, its segments joined by `/` and a `/` after
/// it once it is not empty (git's `base`), and a frame for each tree on that path.
struct SparseWalk<'p, N> {
  /// The patterns.
  patterns: &'p Patterns,
  /// The path.
  path: Vec<u8>,
  /// The frames, the tree being walked last.
  frames: Vec<Frame<N>>,
}

/// A tree being walked under a `sparse:oid` filter (git's `struct frame`).
struct Frame<N> {
  /// The tree.
  tree: N,
  /// The path's length before the tree's name.
  base: usize,
  /// Whether a path below it that the patterns leave undecided is matched (`default_match`).
  matched: bool,
  /// Whether a blob below it was left out in this walk (`child_prov_omit`).
  left_out_below: bool,
  /// Its entries not yet met, with their names, the next last.
  entries: Vec<(N, Vec<u8>)>,
}

/// The patterns the walk asks of each path: the request's, when its filter is `sparse:oid` alone (a combination holding
/// one, or one without its patterns, was refused before the walk).
fn sparse_patterns<'r>(request: &Request<'r>) -> Option<&'r Patterns> {
  match request.filter {
    Some(Filter::SparseOid(_)) => request.sparse,
    _ => None,
  }
}

/// Whether a path is matched: by its own verdict, or by `inherited`, the verdict of the tree it is in, when its own is
/// undecided.
fn decided(verdict: Verdict, inherited: bool) -> bool {
  match verdict {
    Verdict::Matched => true,
    Verdict::NotMatched => false,
    Verdict::Undecided => inherited,
  }
}

/// Charges `bytes` kept to `admission`.
fn charge_bytes(admission: &mut Admission, bytes: usize) -> Result<(), Refusal> {
  admission
    .charge(crate::to_u64(bytes))
    .map_err(Refusal::PastAdmission)
}

impl<N: Copy + Ord + Into<u64>> Chosen<'_, N> {
  /// Chooses `node`.
  fn take(&mut self, node: N, admission: &mut Admission) -> Result<(), Refusal> {
    self.objects.insert(node, admission)?;
    Ok(())
  }

  /// Whether the filter admits an object of `kind` and `size` met at `depth`.
  fn admits(&self, kind: Kind, size: u64, depth: u64) -> bool {
    self
      .filter
      .is_none_or(|filter| filter.admits(kind, size, depth))
  }

  /// A sent commit's root tree, at depth 0, and what lies below it.
  fn root<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    tree: N,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    if self.left_out.contains(tree) {
      return Ok(());
    }
    if let Some(patterns) = self.sparse {
      return self.walk_sparse(graph, tree, patterns, admission, counters);
    }
    if self.admits(Kind::Tree, 0, 0) {
      self.take(tree, admission)?;
    }
    self.open(graph, tree, 1, admission, counters)
  }

  /// A `sparse:oid` filter's walk from `root`, a sent commit's tree or a wanted one (git's `process_tree` under
  /// `filter_sparse`; the module doc's item 3), without recursion: a frame for each tree on the path walked.
  fn walk_sparse<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    root: N,
    patterns: &Patterns,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    let mut walk = SparseWalk {
      patterns,
      path: Vec::new(),
      frames: Vec::new(),
    };
    self.enter(graph, &mut walk, root, &[], admission, counters)?;
    while let Some(frame) = walk.frames.last_mut() {
      match frame.entries.pop() {
        Some((entry, name)) => match graph.kind(entry) {
          Kind::Tree => self.enter(graph, &mut walk, entry, &name, admission, counters)?,
          Kind::Blob => self.blob(&mut walk, entry, &name, admission)?,
          Kind::Commit | Kind::Tag => {}
        },
        None => self.leave(&mut walk, admission)?,
      }
    }
    Ok(())
  }

  /// Meets `tree`, named `name` in the tree on top of the walk (git's `LOFS_BEGIN_TREE`): sends it, decides the verdict
  /// a path below it that the patterns leave undecided takes, and keeps a frame for it; nothing when the client holds it
  /// or it is done.
  fn enter<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    walk: &mut SparseWalk<'_, N>,
    tree: N,
    name: &[u8],
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    if self.left_out.contains(tree) || self.done.contains(tree) {
      return Ok(());
    }
    let base = walk.path.len();
    charge_bytes(admission, name.len().saturating_add(1))?;
    walk.path.extend_from_slice(name);
    let inherited = walk.frames.last().is_some_and(|frame| frame.matched);
    let matched = decided(walk.patterns.decide(&walk.path, true), inherited);
    self.take(tree, admission)?;
    if !walk.path.is_empty() {
      walk.path.push(SLASH);
    }
    let mut entries: Vec<(N, Vec<u8>)> = graph.named_entries(tree).collect();
    admission
      .charge_items::<(N, Vec<u8>)>(entries.len())
      .map_err(Refusal::PastAdmission)?;
    charge_bytes(admission, entries.iter().map(|(_, name)| name.len()).sum())?;
    charge::<Frame<N>>(admission)?;
    entries.reverse();
    counters.trees = counters.trees.saturating_add(1);
    walk.frames.push(Frame {
      tree,
      base,
      matched,
      left_out_below: false,
      entries,
    });
    Ok(())
  }

  /// Meets `blob`, named `name` in the tree on top of the walk (git's `LOFS_BLOB`): sent when its path is matched;
  /// otherwise left out for now, its tree told, and asked again wherever else it is met.
  fn blob(
    &mut self,
    walk: &mut SparseWalk<'_, N>,
    blob: N,
    name: &[u8],
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    if self.left_out.contains(blob) || self.objects.contains(blob) {
      return Ok(());
    }
    let Some(frame) = walk.frames.last_mut() else {
      return Ok(());
    };
    let base = walk.path.len();
    charge_bytes(admission, name.len())?;
    walk.path.extend_from_slice(name);
    let matched = decided(walk.patterns.decide(&walk.path, false), frame.matched);
    walk.path.truncate(base);
    if matched {
      return self.take(blob, admission);
    }
    frame.left_out_below = true;
    Ok(())
  }

  /// Leaves the tree on top of the walk (git's `LOFS_END_TREE`): done when no blob below it was left out, and its tree
  /// told when one was.
  fn leave(
    &mut self,
    walk: &mut SparseWalk<'_, N>,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    let Some(frame) = walk.frames.pop() else {
      return Ok(());
    };
    walk.path.truncate(frame.base);
    if let Some(parent) = walk.frames.last_mut() {
      parent.left_out_below |= frame.left_out_below;
    }
    if !frame.left_out_below {
      self.done.insert(frame.tree, admission)?;
    }
    Ok(())
  }

  /// Opens `tree`, its entries met at `depth`, and every tree below it, without recursion.
  fn open<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    tree: N,
    depth: u64,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    let mut stack = vec![(tree, depth)];
    while let Some((tree, depth)) = stack.pop() {
      if !self.filter.is_none_or(|filter| filter.reaches(depth)) {
        continue;
      }
      let opened_before = match &self.opened {
        Opened::Once(opened) => opened.contains(tree),
        Opened::AtDepth(opened) => opened.get(&tree).is_some_and(|before| *before <= depth),
      };
      if opened_before {
        continue;
      }
      match &mut self.opened {
        Opened::Once(opened) => {
          opened.insert(tree, admission)?;
        }
        Opened::AtDepth(opened) => {
          if opened.insert(tree, depth).is_none() {
            charge::<(N, u64)>(admission)?;
          }
        }
      }
      counters.trees = counters.trees.saturating_add(1);
      for entry in graph
        .entries(tree)
        .filter(|entry| !self.left_out.contains(*entry))
      {
        let kind = graph.kind(entry);
        let size = if kind == Kind::Blob {
          graph.size(entry)
        } else {
          0
        };
        if self.admits(kind, size, depth) {
          self.take(entry, admission)?;
        }
        if kind == Kind::Tree {
          stack.push((entry, depth.saturating_add(1)));
          self.stack.reach::<(N, u64)>(stack.len(), admission)?;
        }
      }
    }
    Ok(())
  }

  /// A want that is no commit, or a tag: the want and every tag of its chain, whatever the filter; a wanted tree's
  /// entries met at depth 0.
  fn want<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    want: N,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    let mut current = want;
    loop {
      match graph.kind(current) {
        Kind::Tag => {
          self.take(current, admission)?;
          let Some(next) = graph.target(current) else {
            return Ok(());
          };
          current = next;
        }
        Kind::Tree => {
          self.take(current, admission)?;
          if let Some(patterns) = self.sparse {
            return self.walk_sparse(graph, current, patterns, admission, counters);
          }
          return self.open(graph, current, 0, admission, counters);
        }
        Kind::Blob => return self.take(current, admission),
        Kind::Commit => return Ok(()),
      }
    }
  }

  /// `include-tag`: each annotated tag whose chain ends at an object the pack carries, with its chain.
  fn tags<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    carried: &Held,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    for tag in graph.tags() {
      charge::<N>(admission)?;
      let mut chain = vec![tag];
      let mut current = tag;
      let mut ends = None;
      while graph.kind(current) == Kind::Tag {
        let Some(next) = graph.target(current) else {
          break;
        };
        if graph.kind(next) == Kind::Tag {
          charge::<N>(admission)?;
          chain.push(next);
        } else {
          ends = Some(next);
        }
        current = next;
      }
      let packed = ends.is_some_and(|end| self.objects.contains(end) || carried.contains(end));
      if packed {
        for link in chain {
          self.take(link, admission)?;
        }
      }
    }
    Ok(())
  }
}
