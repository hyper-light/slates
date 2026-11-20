//! Shallow histories (A-615 part 2; `upload-pack.c` `process_shallow`, `deepen`, `deepen_by_rev_list`, `send_shallow`,
//! `send_unshallow` and `send_shallow_info`; `shallow.c` `get_shallow_commits` and `get_shallow_commits_by_rev_list`;
//! git v2.50.1).
//!
//! - `deepen <n>`: the depth of a commit is 1 at a want and one more per parent step, its least over every path; the
//!   commits at depth `n` are shallow and the ones above them whole (`get_shallow_commits`, whose depth-first walk keeps
//!   each commit's least depth; here a breadth-first walk finds the same). A commit at depth `n` is shallow even when it
//!   has no parent, as git marks it.
//! - `deepen-relative`: the same, measured from the client's shallow commits that the refs reach (`get_reachable_list`),
//!   to depth `n + 1`.
//! - [`super::INFINITE_DEPTH`]: every client shallow commit is made whole, and none is new.
//! - `deepen-since` and `deepen-not`: the commits `rev-list` selects, those the wants reach that are neither older than
//!   the time nor reached from a commit that is (`--max-age` marks such a commit uninteresting, and its parents with it)
//!   nor reached from the `deepen-not` objects; a selected commit with a parent not selected is shallow. Selecting none
//!   is refused, as git dies on it.
//!
//! A shallow commit is sent as `shallow` unless the client already holds it shallow; a client shallow commit the cut
//! makes whole is sent as `unshallow`, and its parents are wanted, itself treated as a have (`extra_edge_obj`). Every
//! client shallow commit and every new one is a graft for the walk. A client that is shallow and asks no deepening gets
//! an empty shallow-info section, as git sends one. A client's shallow line naming an object the store does not hold is
//! left out, as git leaves it out, and one naming an object that is no commit is refused.

use std::collections::BTreeSet;

use super::paint::{HAVE, Paint, WANT};
use super::ready::peel;
use super::{
  Counters, Deepen, Graph, Held, INFINITE_DEPTH, Peak, Refusal, Request, ShallowInfo, charge,
};
use crate::Admission;
use crate::object_name::ObjectId;

/// A deepening's answer: the shallow commits, and the commits kept whole.
type ShallowAndWhole<N> = (BTreeSet<N>, Held);

/// What a request's shallow lines and deepening make of the walk.
pub(super) struct Cut<N> {
  /// The shallow-info section, when one is due.
  pub(super) info: Option<ShallowInfo>,
  /// The commits whose parents the walk does not follow: the client's shallow commits and the new ones.
  pub(super) grafts: BTreeSet<N>,
  /// Commits wanted besides the request's: the parents of each commit made whole.
  pub(super) wants: Vec<N>,
  /// Commits the client has besides its haves: each commit made whole.
  pub(super) haves: Vec<N>,
}

/// The client's shallow commits the store holds, each once; refused at one that is no commit.
pub(super) fn client<S: Graph>(
  graph: &S,
  ids: &[ObjectId],
  admission: &mut Admission,
) -> Result<Vec<S::Node>, Refusal> {
  let mut seen = BTreeSet::new();
  let mut commits = Vec::new();
  for id in ids {
    let Some(node) = graph.node(id) else {
      continue;
    };
    if graph.commit(node).is_none() {
      return Err(Refusal::ShallowNotCommit { id: *id });
    }
    if seen.insert(node) {
      charge::<(S::Node, S::Node)>(admission)?;
      commits.push(node);
    }
  }
  Ok(commits)
}

/// A request's deepening, owned so the cut outlives every lending of the graph (A-745): its `deepen-not` objects resolved
/// as the request named them, a name the store does not hold kept to be refused when the cut begins.
#[derive(Clone, Debug)]
pub(super) enum Deepening<N> {
  /// `deepen <depth>`, `deepen-relative`.
  Depth {
    /// The depth.
    depth: u32,
    /// `deepen-relative`.
    relative: bool,
  },
  /// `deepen-since`, `deepen-not`.
  Cut {
    /// `deepen-since <time>`.
    since: Option<u64>,
    /// Each `deepen-not`'s object, or the name the store does not hold.
    not: Vec<Result<N, ObjectId>>,
  },
}

impl<N> Deepening<N> {
  /// `request`'s deepening, if any, its `deepen-not` names looked up and charged.
  pub(super) fn of<S: Graph<Node = N>>(
    graph: &S,
    request: &Request<'_>,
    admission: &mut Admission,
  ) -> Result<Option<Deepening<N>>, Refusal> {
    Ok(match request.deepen {
      None => None,
      Some(Deepen::Depth { depth, relative }) => Some(Deepening::Depth { depth, relative }),
      Some(Deepen::Cut { since, not }) => {
        admission
          .charge_items::<Result<N, ObjectId>>(not.len())
          .map_err(Refusal::PastAdmission)?;
        let not = not.iter().map(|id| graph.node(id).ok_or(*id)).collect();
        Some(Deepening::Cut { since, not })
      }
    })
  }
}

/// The cut a request makes, made whole ([`Cutting`] stepped to its end).
pub(super) fn cut<S: Graph>(
  graph: &S,
  request: &Request<'_>,
  wants: &[S::Node],
  client: &[S::Node],
  admission: &mut Admission,
  counters: &mut Counters,
) -> Result<Cut<S::Node>, Refusal> {
  let deepening = Deepening::of(graph, request, admission)?;
  let mut cutting = Cutting::begin(graph, deepening, wants, client, admission)?;
  loop {
    if let Some(cut) = cutting.unit(graph, client, admission, counters)? {
      return Ok(cut);
    }
  }
}

/// The cut being made a unit at a time (A-745, §4.17.1 "A fetch's answer" rule 3): a commit of the deepening's walk a
/// unit, or a ref's tip, the walk keeping its place across steps, each lent the graph anew.
pub(super) struct Cutting<N> {
  /// The client's shallow commits, the walk's grafts.
  grafts: BTreeSet<N>,
  /// Where the cut is.
  walk: Walk<N>,
}

/// Where a cut is.
enum Walk<N> {
  /// Made: its sections and grafts known.
  Made(Option<Cut<N>>),
  /// `deepen-relative`: the client's shallow commits the refs reach, searched from each ref's tip down to the least
  /// generation among them.
  Tips {
    /// The depth asked.
    depth: u32,
    /// The ref whose tip is taken next, by its index among the refs.
    next: usize,
    /// The least generation among the client's shallow commits.
    floor: u64,
    /// The commits met, as runs or bits (A-745 rule 1: a bit or a word a commit, never an entry).
    seen: Held,
    /// The commits to search.
    stack: Vec<N>,
    /// That stack's cost: its deepest.
    stack_peak: Peak,
  },
  /// `deepen <depth>`: breadth first from the roots, a level at a time, so each commit is met first at its least depth
  /// (A-745 rule 1: the commits met and kept whole held as runs or bits, never an entry a commit, so a deepening
  /// reaching most of a history, as `--deepen 100` reaches 24,885 of git v2.0.0's 36,430 commits, fits one request's
  /// room).
  Depth {
    /// The depth at which a commit is shallow.
    limit: u32,
    /// The depth of the commits in `frontier`.
    level: u32,
    /// The commits at `level` still to take.
    frontier: Vec<N>,
    /// The commits met at the next level.
    next: Vec<N>,
    /// The two levels' cost: their most.
    peak: Peak,
    /// Every commit met.
    seen: Held,
    /// The shallow commits and those kept whole.
    found: ShallowAndWhole<N>,
  },
  /// `deepen-since`, `deepen-not`: the walk painting what `rev-list` selects.
  Paint {
    /// The walk.
    paint: Paint<N>,
    /// `deepen-since <time>`.
    since: Option<u64>,
  },
  /// The selected commits judged, shallow or whole: the taken commit at or after `next` next.
  Select {
    /// The walk.
    paint: Paint<N>,
    /// The next position looked at.
    next: u64,
    /// Whether any commit was selected.
    any: bool,
    /// The shallow commits and those kept whole.
    found: ShallowAndWhole<N>,
  },
  /// The walk done: the sections made from what it found.
  Finish(ShallowAndWhole<N>),
}

impl<N: Copy + Ord + Into<u64> + TryFrom<u64>> Cutting<N> {
  /// The cut `deepening` makes of the walk from `wants`, a client holding `client` shallow, begun: everything the
  /// request bounds (the roots, the `deepen-not` objects painted), the walk left to [`Cutting::unit`].
  pub(super) fn begin<S: Graph<Node = N>>(
    graph: &S,
    deepening: Option<Deepening<N>>,
    wants: &[N],
    client: &[N],
    admission: &mut Admission,
  ) -> Result<Cutting<N>, Refusal> {
    let grafts: BTreeSet<N> = client.iter().copied().collect();
    let walk = match deepening {
      None => {
        let info = (!client.is_empty()).then(ShallowInfo::default);
        Walk::Made(Some(Cut {
          info,
          grafts: grafts.clone(),
          wants: Vec::new(),
          haves: Vec::new(),
        }))
      }
      Some(Deepening::Depth { depth: 0, .. }) => return Err(Refusal::ZeroDepth),
      Some(Deepening::Depth {
        depth: INFINITE_DEPTH,
        ..
      }) => {
        let mut whole = Held::default();
        for node in &grafts {
          whole.insert(*node, admission)?;
        }
        Walk::Finish((BTreeSet::new(), whole))
      }
      Some(Deepening::Depth {
        depth,
        relative: true,
      }) => Walk::Tips {
        depth,
        next: 0,
        floor: client
          .iter()
          .map(|node| super::ready::generation(graph, *node))
          .min()
          .unwrap_or(u64::MAX),
        seen: Held::default(),
        stack: Vec::new(),
        stack_peak: Peak::default(),
      },
      Some(Deepening::Depth {
        depth,
        relative: false,
      }) => {
        let roots: Vec<N> = wants
          .iter()
          .filter_map(|want| peel(graph, *want))
          .filter(|node| graph.commit(*node).is_some())
          .collect();
        by_depth(roots, depth, admission)?
      }
      Some(Deepening::Cut { since, not }) => {
        let mut paint = Paint::new();
        for want in wants.iter().filter_map(|want| peel(graph, *want)) {
          if graph.commit(want).is_some() {
            paint.paint(graph, want, WANT, admission)?;
          }
        }
        for node in not {
          let node = node.map_err(|id| Refusal::DeepenNotHeld { id })?;
          if let Some(commit) = peel(graph, node).filter(|node| graph.commit(*node).is_some()) {
            paint.paint(graph, commit, HAVE, admission)?;
          }
        }
        Walk::Paint { paint, since }
      }
    };
    Ok(Cutting { grafts, walk })
  }

  /// One unit of the cut: the cut once it is made, `None` while it is not.
  pub(super) fn unit<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    client: &[N],
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<Option<Cut<N>>, Refusal> {
    match &mut self.walk {
      Walk::Made(cut) => return Ok(cut.take()),
      Walk::Tips {
        depth,
        next,
        floor,
        seen,
        stack,
        stack_peak,
      } => {
        if let Some(commit) = stack.pop() {
          if super::ready::generation(graph, commit) >= *floor && seen.insert(commit, admission)? {
            stack.extend(graph.parents(commit));
            stack_peak.reach::<N>(stack.len(), admission)?;
          }
          return Ok(None);
        }
        if let Some((at, tip)) = graph.tips_from(*next).next() {
          *next = at.saturating_add(1);
          if let Some(commit) = peel(graph, tip).filter(|node| graph.commit(*node).is_some()) {
            stack.push(commit);
            stack_peak.reach::<N>(stack.len(), admission)?;
          }
          return Ok(None);
        }
        let roots: Vec<N> = client
          .iter()
          .copied()
          .filter(|node| seen.contains(*node))
          .collect();
        self.walk = by_depth(roots, depth.saturating_add(1), admission)?;
      }
      Walk::Depth {
        limit,
        level,
        frontier,
        next,
        peak,
        seen,
        found,
      } => {
        let Some(commit) = frontier.pop() else {
          if next.is_empty() {
            let found = core::mem::replace(found, (BTreeSet::new(), Held::default()));
            self.walk = Walk::Finish(found);
          } else {
            core::mem::swap(frontier, next);
            *level = level.saturating_add(1);
          }
          return Ok(None);
        };
        if *level >= *limit {
          if found.0.insert(commit) {
            charge::<N>(admission)?;
          }
          return Ok(None);
        }
        found.1.insert(commit, admission)?;
        for parent in graph.parents(commit) {
          if seen.insert(parent, admission)? {
            next.push(parent);
          }
        }
        peak.reach::<N>(frontier.len().saturating_add(next.len()), admission)?;
      }
      Walk::Paint { paint, since } => {
        let since = *since;
        let old = |commit| {
          since.is_some_and(|time| graph.commit(commit).is_some_and(|row| row.date < time))
        };
        if !paint.step(graph, &BTreeSet::new(), &old, admission, counters)? {
          let paint = core::mem::replace(paint, Paint::new());
          self.walk = Walk::Select {
            paint,
            next: 0,
            any: false,
            found: (BTreeSet::new(), Held::default()),
          };
        }
      }
      Walk::Select {
        paint,
        next,
        any,
        found,
      } => {
        let Some(position) = paint.taken_from(*next) else {
          if !*any {
            return Err(Refusal::NothingSelected);
          }
          let found = core::mem::take(found);
          self.walk = Walk::Finish(found);
          return Ok(None);
        };
        *next = position.saturating_add(1);
        let Ok(commit) = N::try_from(position) else {
          return Ok(None);
        };
        if !paint.is_chosen(position) {
          return Ok(None);
        }
        *any = true;
        charge::<(N, N)>(admission)?;
        if graph
          .parents(commit)
          .any(|parent| !paint.is_chosen(parent.into()))
        {
          found.0.insert(commit);
        } else {
          found.1.insert(commit, admission)?;
        }
      }
      Walk::Finish(found) => {
        let (shallow, whole) = core::mem::take(found);
        let grafts = core::mem::take(&mut self.grafts);
        let cut = finish(graph, client, grafts, shallow, &whole, admission)?;
        self.walk = Walk::Made(None);
        return Ok(Some(cut));
      }
    }
    Ok(None)
  }
}

/// `deepen <depth>` (or `deepen-relative`'s `depth + 1`) from `roots`, begun: each root at depth 1.
fn by_depth<N: Copy + Ord + Into<u64>>(
  roots: Vec<N>,
  limit: u32,
  admission: &mut Admission,
) -> Result<Walk<N>, Refusal> {
  let mut seen = Held::default();
  let mut frontier = Vec::new();
  for root in roots {
    if seen.insert(root, admission)? {
      frontier.push(root);
    }
  }
  let mut peak = Peak::default();
  peak.reach::<N>(frontier.len(), admission)?;
  Ok(Walk::Depth {
    limit,
    level: 1,
    frontier,
    next: Vec::new(),
    peak,
    seen,
    found: (BTreeSet::new(), Held::default()),
  })
}

/// The cut's sections and grafts, from the new shallow commits and the commits the cut keeps whole.
fn finish<S: Graph>(
  graph: &S,
  client: &[S::Node],
  mut grafts: BTreeSet<S::Node>,
  shallow: BTreeSet<S::Node>,
  whole: &Held,
  admission: &mut Admission,
) -> Result<Cut<S::Node>, Refusal> {
  let made_whole: Vec<S::Node> = client
    .iter()
    .copied()
    .filter(|node| whole.contains(*node))
    .collect();
  let mut wants = Vec::new();
  for parent in made_whole.iter().flat_map(|node| graph.parents(*node)) {
    charge::<S::Node>(admission)?;
    wants.push(parent);
  }
  let mut info = ShallowInfo {
    shallow: shallow
      .iter()
      .filter(|node| !client.contains(node))
      .map(|node| graph.id(*node))
      .collect(),
    unshallow: made_whole.iter().map(|node| graph.id(*node)).collect(),
  };
  info.shallow.sort();
  info.unshallow.sort();
  for node in shallow {
    if grafts.insert(node) {
      charge::<S::Node>(admission)?;
    }
  }
  Ok(Cut {
    info: Some(info),
    grafts,
    wants,
    haves: made_whole,
  })
}
