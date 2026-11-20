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
use crate::object_name::{Kind, ObjectId};
use crate::positions::Positions;
use crate::{Admission, Hold};

/// Format: the byte between a path's segments.
const SLASH: u8 = b'/';

/// The sides of a pack: what the request wants and has, and the cut its shallow lines make, owned by the selection so it
/// outlives every lending of the graph (A-745).
pub(super) struct Sides<N> {
  /// The wants.
  pub(super) wants: Vec<N>,
  /// The wants a run answers.
  pub(super) one_round: Vec<N>,
  /// The common haves.
  pub(super) common: Vec<N>,
  /// The cut.
  pub(super) cut: Cut<N>,
}

/// What the selection reads of the request, owned (A-745): the filter, a `sparse:oid` filter's patterns, `thin-pack`,
/// `include-tag`, and the haves as sent, which a thin pack in a shallow exchange lists.
struct Asked {
  /// The filter.
  filter: Option<Filter>,
  /// The patterns of a `sparse:oid` filter, alone (a combination holding one, or one without its patterns, was refused
  /// before the walk).
  sparse: Option<Patterns>,
  /// `thin-pack`.
  thin: bool,
  /// `include-tag`.
  include_tag: bool,
  /// The haves, as sent.
  haves: Vec<ObjectId>,
}

impl Asked {
  /// What `request` asks of the selection, its haves charged as they are copied.
  fn of(request: &Request<'_>, admission: &mut Admission) -> Result<Asked, Refusal> {
    admission
      .charge_items::<ObjectId>(request.haves.len())
      .map_err(Refusal::PastAdmission)?;
    let sparse = match request.filter {
      Some(Filter::SparseOid(_)) => request.sparse.cloned(),
      _ => None,
    };
    Ok(Asked {
      filter: request.filter.cloned(),
      sparse,
      thin: request.thin,
      include_tag: request.include_tag,
      haves: request.haves.to_vec(),
    })
  }
}

/// The objects a pack carries one by one, chosen a unit at a time (A-745, §4.17.1 "A fetch's answer" rule 3: every slice a
/// turn at a time, the graph lent for that turn alone): each [`Selecting::step`] does units until its owner's quantum is
/// spent, at least one, a commit taken from the walk's queue or an entry taken from a stack, and keeps its place across
/// steps. The module doc's rules, in order: the commits (the walk), what the client holds (the edges' trees, then the
/// trees and blobs a have names), the sent commits' trees, the wants, and `include-tag`.
pub(super) struct Selecting<N, G> {
  /// What the request asks.
  asked: Asked,
  /// What the request wants and has.
  sides: Sides<N>,
  /// The runs the pack carries whole.
  runs: Vec<Run<G>>,
  /// Where the selection is.
  stage: Stage,
  /// The commit walk.
  paint: Paint<N>,
  /// The commits sent, once the walk is done.
  sent: Positions,
  /// What the client holds of trees and blobs.
  held: Held,
  /// The trees being marked held, not yet taken.
  mark: Vec<N>,
  /// That stack's cost: its deepest.
  mark_peak: Peak,
  /// The objects chosen, and the tree walks choosing them.
  chosen: Sets<N>,
}

/// Where a selection is (the stages of [`Selecting`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
  /// The commit walk.
  Commits,
  /// The edges' trees marked held: the sent commit at or after this position next.
  Edges {
    /// The next position looked at.
    next: u64,
  },
  /// The trees a thin pack in a shallow exchange leaves out, and the trees and blobs the haves name, marked held.
  Haves,
  /// The sent commits and their trees chosen: the sent commit at or after this position next.
  Choose {
    /// The next position looked at.
    next: u64,
  },
  /// The wants not answered by a run: this one next.
  Wants {
    /// The index of the next want.
    at: usize,
  },
  /// `include-tag`.
  Tags,
  /// Done: every object chosen.
  Done,
}

/// The objects chosen so far, and the walks of trees not yet done.
struct Sets<N> {
  /// The objects chosen.
  objects: Held,
  /// Each tree opened.
  opened: Opened<N>,
  /// Under a `sparse:oid` filter, each tree no blob below which was left out: never walked again.
  done: Held,
  /// Trees to open, each with the depth its entries are met at.
  open: Vec<(N, u64)>,
  /// That stack's cost: its deepest.
  open_peak: Peak,
  /// The `sparse:oid` walk's frames, the tree being walked last: none outside one.
  frames: Vec<Frame<N>>,
  /// The `sparse:oid` walk's path: the walked tree's, its segments joined by `/` and a `/` after it once it is not empty
  /// (git's `base`).
  path: Vec<u8>,
  /// The path's cost: its longest.
  path_peak: Peak,
}

/// The trees a walk opened: as runs where each opens once (every filter but `tree:`), with the depth each was last met
/// at where that depth decides what is sent below it (a `tree:` filter, which reopens a tree met again shallower).
enum Opened<N> {
  /// Each opened once.
  Once(Held),
  /// Each with the least depth its entries were met at.
  AtDepth(BTreeMap<N, u64>),
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
  /// The bytes its entries hold, given back as it is left.
  held: Hold,
}

/// What the choosing reads besides its own sets: the filter, the patterns, and what the client holds.
struct View<'v> {
  /// The filter.
  filter: Option<&'v Filter>,
  /// A `sparse:oid` filter's patterns.
  patterns: Option<&'v Patterns>,
  /// What the client holds.
  held: &'v Held,
}

impl View<'_> {
  /// Whether the filter admits an object of `kind` and `size` met at `depth`.
  fn admits(&self, kind: Kind, size: u64, depth: u64) -> bool {
    self
      .filter
      .is_none_or(|filter| filter.admits(kind, size, depth))
  }
}

impl<N: Copy + Ord + Into<u64> + TryFrom<u64>, G> Selecting<N, G> {
  /// Begins the selection of what `request` asks over `sides`: the walk seeded with the wants not answered by a run (and
  /// the parents of commits a cut made whole) as wanted, and the common haves (and the commits a cut made whole, and the
  /// wants a run answers) as had.
  pub(super) fn begin<S: Graph<Node = N, Green = G>>(
    graph: &S,
    request: &Request<'_>,
    sides: Sides<N>,
    runs: Vec<Run<G>>,
    admission: &mut Admission,
  ) -> Result<Selecting<N, G>, Refusal> {
    let asked = Asked::of(request, admission)?;
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
      .chain(&sides.one_round);
    for have in had.filter_map(|node| commit_of(graph, *node)) {
      paint.paint(graph, have, HAVE, admission)?;
    }
    let depth_decides = asked
      .filter
      .as_ref()
      .is_some_and(|filter| !filter.is_per_object());
    Ok(Selecting {
      asked,
      sides,
      runs,
      stage: Stage::Commits,
      paint,
      sent: Positions::new(),
      held: Held::default(),
      mark: Vec::new(),
      mark_peak: Peak::default(),
      chosen: Sets {
        objects: Held::default(),
        opened: if depth_decides {
          Opened::AtDepth(BTreeMap::new())
        } else {
          Opened::Once(Held::default())
        },
        done: Held::default(),
        open: Vec::new(),
        open_peak: Peak::default(),
        frames: Vec::new(),
        path: Vec::new(),
        path_peak: Peak::default(),
      },
    })
  }

  /// Does units of the selection until `spent` says the owner's quantum is spent, at least one: the objects chosen, as
  /// runs, once the last is done; `None` while more are left.
  pub(super) fn step<S: Graph<Node = N, Green = G>>(
    &mut self,
    graph: &S,
    admission: &mut Admission,
    counters: &mut Counters,
    spent: &mut dyn FnMut() -> bool,
  ) -> Result<Option<Positions>, Refusal> {
    loop {
      if self.unit(graph, admission, counters)? {
        return Ok(Some(
          core::mem::take(&mut self.chosen.objects).into_positions(),
        ));
      }
      if spent() {
        return Ok(None);
      }
    }
  }

  /// One unit: a stack's next entry where a walk is under way, else the stage's next; whether the selection is done.
  fn unit<S: Graph<Node = N, Green = G>>(
    &mut self,
    graph: &S,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<bool, Refusal> {
    if let Some(node) = self.mark.pop() {
      self.mark_one(graph, node, admission)?;
      return Ok(false);
    }
    let view = View {
      filter: self.asked.filter.as_ref(),
      patterns: self.asked.sparse.as_ref(),
      held: &self.held,
    };
    if self.chosen.walking() {
      self.chosen.walk_one(graph, &view, admission, counters)?;
      return Ok(false);
    }
    match self.stage {
      Stage::Commits => {
        if !self.paint.step(
          graph,
          &self.sides.cut.grafts,
          &|_| false,
          admission,
          counters,
        )? {
          self.sent = self.paint.chosen();
          admission
            .charge_items::<(u64, u64)>(self.sent.runs())
            .map_err(Refusal::PastAdmission)?;
          self.stage = Stage::Edges { next: 0 };
        }
      }
      Stage::Edges { next } => match self.sent.next_from(next) {
        Some(position) => {
          self.stage = Stage::Edges {
            next: position.saturating_add(1),
          };
          self.edges_of(graph, position, admission)?;
        }
        None => self.stage = Stage::Haves,
      },
      Stage::Haves => {
        self.haves(graph, admission)?;
        self.stage = Stage::Choose { next: 0 };
      }
      Stage::Choose { next } => match self.sent.next_from(next) {
        Some(position) => {
          self.stage = Stage::Choose {
            next: position.saturating_add(1),
          };
          if let Ok(commit) = N::try_from(position) {
            if view.admits(Kind::Commit, 0, 0) {
              self.chosen.objects.insert(commit, admission)?;
            }
            if let Some(row) = graph.commit(commit) {
              self
                .chosen
                .root(graph, &view, row.tree, admission, counters)?;
            }
          }
        }
        None => self.stage = Stage::Wants { at: 0 },
      },
      Stage::Wants { at } => {
        let want = self
          .sides
          .wants
          .iter()
          .skip(at)
          .enumerate()
          .find(|(_, want)| !self.sides.one_round.contains(want));
        match want {
          Some((past, want)) => {
            self.stage = Stage::Wants {
              at: at.saturating_add(past).saturating_add(1),
            };
            self.chosen.want(graph, &view, *want, admission, counters)?;
          }
          None => self.stage = Stage::Tags,
        }
      }
      Stage::Tags => {
        if self.asked.include_tag {
          let carried = carried_by_runs(graph, &self.runs, view.filter, admission)?;
          self.chosen.tags(graph, &carried, admission)?;
        }
        self.stage = Stage::Done;
      }
      Stage::Done => return Ok(true),
    }
    Ok(false)
  }

  /// The edges of the sent commit at `position`: each had parent's tree marked held, as git leaves out the trees of
  /// `mark_edges_uninteresting`; nothing for a graft.
  fn edges_of<S: Graph<Node = N, Green = G>>(
    &mut self,
    graph: &S,
    position: u64,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    let Ok(commit) = N::try_from(position) else {
      return Ok(());
    };
    if self.sides.cut.grafts.contains(&commit) {
      return Ok(());
    }
    for parent in graph
      .parents(commit)
      .filter(|parent| !self.sent.contains((*parent).into()))
    {
      if let Some(row) = graph.commit(parent) {
        self.mark.push(row.tree);
        self.mark_peak.reach::<N>(self.mark.len(), admission)?;
      }
    }
    Ok(())
  }

  /// The roots of what else the client holds, marked held (the module doc's item 2): for a thin pack in a shallow
  /// exchange, the tree of each had commit git's command line names (each have it lists, each commit a cut made whole);
  /// and each have that is no commit. Bounded by the request.
  fn haves<S: Graph<Node = N, Green = G>>(
    &mut self,
    graph: &S,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    if self.asked.thin && !self.sides.cut.grafts.is_empty() {
      let listed = listed_haves(graph, &self.asked.haves, admission)?;
      for had in listed.iter().chain(&self.sides.cut.haves) {
        if let Some(row) = graph.commit(*had) {
          self.mark.push(row.tree);
        }
      }
    }
    for have in self
      .sides
      .common
      .iter()
      .filter_map(|node| peel(graph, *node))
    {
      if graph.commit(have).is_none() {
        self.mark.push(have);
      }
    }
    self.mark_peak.reach::<N>(self.mark.len(), admission)
  }

  /// Marks `node` held, and queues what a tree holds: a tree already held is skipped whole.
  fn mark_one<S: Graph<Node = N, Green = G>>(
    &mut self,
    graph: &S,
    node: N,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    if self.held.insert(node, admission)? && graph.kind(node) == Kind::Tree {
      self.mark.extend(graph.entries(node));
      self.mark_peak.reach::<N>(self.mark.len(), admission)?;
    }
    Ok(())
  }
}

impl<N: Copy + Ord + Into<u64>> Sets<N> {
  /// Whether a tree walk is under way.
  fn walking(&self) -> bool {
    !self.open.is_empty() || !self.frames.is_empty()
  }

  /// One entry of the walk under way: the next tree to open, or the `sparse:oid` walk's next entry.
  fn walk_one<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    view: &View<'_>,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    if let Some((tree, depth)) = self.open.pop() {
      return self.open_one(graph, view, tree, depth, admission, counters);
    }
    let Some(patterns) = view.patterns else {
      return Ok(());
    };
    let next = self.frames.last_mut().map(|frame| frame.entries.pop());
    match next {
      Some(Some((entry, name))) => match graph.kind(entry) {
        Kind::Tree => self.enter(graph, view, patterns, entry, &name, admission, counters),
        Kind::Blob => self.blob(view, patterns, entry, &name, admission),
        Kind::Commit | Kind::Tag => Ok(()),
      },
      Some(None) => self.leave(admission),
      None => Ok(()),
    }
  }

  /// A sent commit's root tree, at depth 0, its walk begun.
  fn root<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    view: &View<'_>,
    tree: N,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    if view.held.contains(tree) {
      return Ok(());
    }
    if let Some(patterns) = view.patterns {
      return self.enter(graph, view, patterns, tree, &[], admission, counters);
    }
    if view.admits(Kind::Tree, 0, 0) {
      self.objects.insert(tree, admission)?;
    }
    self.open.push((tree, 1));
    self.open_peak.reach::<(N, u64)>(self.open.len(), admission)
  }

  /// Opens `tree`, its entries met at `depth`: each one the filter admits chosen, each tree among them queued.
  fn open_one<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    view: &View<'_>,
    tree: N,
    depth: u64,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    if !view.filter.is_none_or(|filter| filter.reaches(depth)) {
      return Ok(());
    }
    let opened_before = match &self.opened {
      Opened::Once(opened) => opened.contains(tree),
      Opened::AtDepth(opened) => opened.get(&tree).is_some_and(|before| *before <= depth),
    };
    if opened_before {
      return Ok(());
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
      .filter(|entry| !view.held.contains(*entry))
    {
      let kind = graph.kind(entry);
      let size = if kind == Kind::Blob {
        graph.size(entry)
      } else {
        0
      };
      if view.admits(kind, size, depth) {
        self.objects.insert(entry, admission)?;
      }
      if kind == Kind::Tree {
        self.open.push((entry, depth.saturating_add(1)));
        self
          .open_peak
          .reach::<(N, u64)>(self.open.len(), admission)?;
      }
    }
    Ok(())
  }

  /// A want that is no commit, or a tag: the want and every tag of its chain, whatever the filter; a wanted tree's walk
  /// begun with its entries met at depth 0.
  fn want<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    view: &View<'_>,
    want: N,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    let mut current = want;
    loop {
      match graph.kind(current) {
        Kind::Tag => {
          self.objects.insert(current, admission)?;
          let Some(next) = graph.target(current) else {
            return Ok(());
          };
          current = next;
        }
        Kind::Tree => {
          self.objects.insert(current, admission)?;
          if let Some(patterns) = view.patterns {
            return self.enter(graph, view, patterns, current, &[], admission, counters);
          }
          self.open.push((current, 0));
          return self.open_peak.reach::<(N, u64)>(self.open.len(), admission);
        }
        Kind::Blob => {
          self.objects.insert(current, admission)?;
          return Ok(());
        }
        Kind::Commit => return Ok(()),
      }
    }
  }

  /// Meets `tree`, named `name` in the tree on top of the walk (git's `LOFS_BEGIN_TREE`): sends it, decides the verdict
  /// a path below it that the patterns leave undecided takes, and keeps a frame for it, its entries' bytes held until it
  /// is left; nothing when the client holds it or it is done.
  #[allow(
    clippy::too_many_arguments,
    reason = "the walk's own state, the view, the patterns and the tree met"
  )]
  fn enter<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    view: &View<'_>,
    patterns: &Patterns,
    tree: N,
    name: &[u8],
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    if view.held.contains(tree) || self.done.contains(tree) {
      return Ok(());
    }
    let base = self.path.len();
    self.path.extend_from_slice(name);
    let inherited = self.frames.last().is_some_and(|frame| frame.matched);
    let matched = decided(patterns.decide(&self.path, true), inherited);
    self.objects.insert(tree, admission)?;
    if !self.path.is_empty() {
      self.path.push(SLASH);
    }
    self.path_peak.reach::<u8>(self.path.len(), admission)?;
    let mut entries: Vec<(N, Vec<u8>)> = graph.named_entries(tree).collect();
    let bytes = core::mem::size_of::<(N, Vec<u8>)>()
      .saturating_mul(entries.len())
      .saturating_add(entries.iter().map(|(_, name)| name.len()).sum())
      .saturating_add(core::mem::size_of::<Frame<N>>());
    let held = admission
      .hold(crate::to_u64(bytes))
      .map_err(Refusal::PastAdmission)?;
    entries.reverse();
    counters.trees = counters.trees.saturating_add(1);
    self.frames.push(Frame {
      tree,
      base,
      matched,
      left_out_below: false,
      entries,
      held,
    });
    Ok(())
  }

  /// Meets `blob`, named `name` in the tree on top of the walk (git's `LOFS_BLOB`): sent when its path is matched;
  /// otherwise left out for now, its tree told, and asked again wherever else it is met.
  fn blob(
    &mut self,
    view: &View<'_>,
    patterns: &Patterns,
    blob: N,
    name: &[u8],
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    if view.held.contains(blob) || self.objects.contains(blob) {
      return Ok(());
    }
    let base = self.path.len();
    self.path.extend_from_slice(name);
    self.path_peak.reach::<u8>(self.path.len(), admission)?;
    let inherited = self.frames.last().is_some_and(|frame| frame.matched);
    let matched = decided(patterns.decide(&self.path, false), inherited);
    self.path.truncate(base);
    if matched {
      self.objects.insert(blob, admission)?;
      return Ok(());
    }
    if let Some(frame) = self.frames.last_mut() {
      frame.left_out_below = true;
    }
    Ok(())
  }

  /// Leaves the tree on top of the walk (git's `LOFS_END_TREE`): done when no blob below it was left out, and its tree
  /// told when one was; its entries' bytes given back.
  fn leave(&mut self, admission: &mut Admission) -> Result<(), Refusal> {
    let Some(frame) = self.frames.pop() else {
      return Ok(());
    };
    self.path.truncate(frame.base);
    if let Some(parent) = self.frames.last_mut() {
      parent.left_out_below |= frame.left_out_below;
    }
    admission.release(frame.held);
    if !frame.left_out_below {
      self.done.insert(frame.tree, admission)?;
    }
    Ok(())
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
          self.objects.insert(link, admission)?;
        }
      }
    }
    Ok(())
  }
}

/// The commit `node` names through its tags, if it names one.
fn commit_of<S: Graph>(graph: &S, node: S::Node) -> Option<S::Node> {
  peel(graph, node).filter(|peeled| graph.commit(*peeled).is_some())
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

/// Whether a path is matched: by its own verdict, or by `inherited`, the verdict of the tree it is in, when its own is
/// undecided.
fn decided(verdict: Verdict, inherited: bool) -> bool {
  match verdict {
    Verdict::Matched => true,
    Verdict::NotMatched => false,
    Verdict::Undecided => inherited,
  }
}
