//! The connectivity check (A-620 part 1; §4.17.2 "A push", item 5: "every object a new commit, tree
//! or tag names is either in the push or in the repository's index", in time "proportional to the
//! new objects", walking no history; git's `check_connected` and `set_connectivity_errors`,
//! v2.50.1, which ask `rev-list --objects` for the new tips not reachable from any ref).
//!
//! The walk is of the pushed objects alone. An object the repository holds ends it, since nothing
//! enters the repository before its push has passed this check. Each pushed object names others: a
//! commit its tree and parents, a tree its entries (a submodule's commit excepted, as git's walk
//! excepts it: it lies in another repository), a tag its object. A name that is neither pushed nor
//! held makes the object, and every pushed object that reaches it, incomplete: `Shallow` when it is
//! a parent of a commit the client called shallow (the push would make the repository shallow),
//! `Missing` otherwise. An object that cannot be read is `Missing`, since what it names cannot be
//! known.
//!
//! **Bounds.** Every object is read twice, once to count the edges among the pushed objects and
//! once to keep them, so the tables are charged exactly before they are made. A read's own
//! transient structures (A-590's header and entry lists) are a small multiple of the object's
//! bytes, which the pack's admission already charged, and are freed before the next read, so they
//! are not charged again. The worst state is carried back over the edges with an explicit stack,
//! each object finished once.

use super::{Pushed, Refusal, Repository, Request, reserve};
use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::{self, ObjectRefused, commit::Commit, tag::Tag, tree::Tree};
use crate::store::hook_view::Quantum;
use crate::store::learn::Clock;
use crate::unpack::Placed;

/// Whether a command's new tip is connected; worse states order later.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Connected {
  /// Everything it reaches is pushed or held.
  #[default]
  Complete,
  /// It reaches a commit the client called shallow whose parent nothing holds (git's "shallow
  /// update not allowed").
  Shallow,
  /// It reaches an object nothing holds, or one that cannot be read (git's "missing necessary
  /// objects").
  Missing,
}

/// Where a walk of one pushed object stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
  /// Not reached.
  New,
  /// On the stack.
  Open,
  /// Its state is final.
  Done,
}

/// Each command's connectivity, in order: a delete is `Complete`; a new tip pushed has its walk's
/// state; a new tip not pushed is `Complete` when the repository holds it and `Missing` otherwise.
/// The walk run whole ([`Walk`], one turn with no end to its quantum).
pub fn connectivity<R: Repository + ?Sized>(
  request: &Request,
  pushed: &Pushed<'_>,
  repository: &R,
  admission: &mut Admission,
) -> Result<Vec<Connected>, Refusal> {
  let mut walk = Walk::new(request, pushed, admission)?;
  let mut stopped = Stopped;
  while !walk.turn(
    (request, pushed, repository),
    admission,
    unbounded(&mut stopped),
  )? {}
  walk.connected(request, pushed, repository)
}

/// A quantum over `stopped` with no end: a whole walk in one turn.
pub(crate) fn unbounded(stopped: &mut Stopped) -> Quantum<'_> {
  Quantum {
    clock: stopped,
    ns: u64::MAX,
  }
}

/// A clock that never moves: a quantum over it never ends.
#[derive(Debug, Default)]
pub(crate) struct Stopped;

impl Clock for Stopped {
  fn now_ns(&mut self) -> u64 {
    0
  }
}

/// Where a walk stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pass {
  /// Each object's own edges out of the pack judged, and the edges among the pushed objects counted.
  States,
  /// The edges among the pushed objects kept, each object's run of targets.
  Edges,
  /// The worst state carried back over the edges.
  Propagate,
  /// Every object's state is final.
  Done,
}

/// The connectivity check made a turn at a time (A-719; the module docs): every pushed object read
/// once a pass in pack order, each turn for its quantum and at least one object, then the states
/// carried back over the edges. Its tables are charged to the admission before they are made, as
/// the whole check charges them.
#[derive(Debug)]
pub(crate) struct Walk {
  /// The commits the client called shallow, sorted.
  shallow: Vec<ObjectId>,
  /// Each pushed object's state, in pack order.
  states: Vec<Connected>,
  /// The edges among the pushed objects, counted by the first pass.
  inside: usize,
  /// Each object's run of targets: its bounds, one more than the objects.
  offsets: Vec<usize>,
  /// The targets, by position.
  targets: Vec<usize>,
  /// The pass under way.
  pass: Pass,
  /// The next object of the pass.
  at: usize,
  /// The propagation's stack (A-722 part 4, the check's longest step at 100 concurrent pushes: the propagation ran whole
  /// in one unit): each open object and its next edge.
  stack: Vec<(usize, usize)>,
  /// Where each object's propagation stands; empty until the propagation begins.
  marks: Vec<Mark>,
}

impl Walk {
  /// A walk of `pushed`, the objects `request`'s pack brought, its state table and the sorted
  /// shallow commits charged to `admission` first.
  pub(crate) fn new(
    request: &Request,
    pushed: &Pushed<'_>,
    admission: &mut Admission,
  ) -> Result<Walk, Refusal> {
    let count = pushed.objects().len();
    admission
      .charge_items::<Connected>(count)
      .map_err(Refusal::PastAdmission)?;
    let mut states = Vec::new();
    reserve(&mut states, count)?;
    states.resize(count, Connected::Complete);
    Ok(Walk {
      shallow: sorted(&request.shallow, admission)?,
      states,
      inside: 0,
      offsets: Vec::new(),
      targets: Vec::new(),
      pass: Pass::States,
      at: 0,
      stack: Vec::new(),
      marks: Vec::new(),
    })
  }

  /// One turn of the walk over `request`'s `pushed` objects and `repository`, for `quantum` and at
  /// least one unit of work: whether the walk is done.
  pub(crate) fn turn<R: Repository + ?Sized>(
    &mut self,
    (request, pushed, repository): (&Request, &Pushed<'_>, &R),
    admission: &mut Admission,
    quantum: Quantum<'_>,
  ) -> Result<bool, Refusal> {
    let Quantum { clock, ns } = quantum;
    let begun = clock.now_ns();
    while self.pass != Pass::Done {
      self.unit((request, pushed, repository), admission)?;
      if clock.now_ns().saturating_sub(begun) >= ns {
        break;
      }
    }
    Ok(self.pass == Pass::Done)
  }

  /// The edges of the object at the pass's position, as `pushed` lends its bytes, handed to `each`:
  /// whether it could be read. A blob names nothing, so its bytes are not read; an object whose
  /// bytes are not lent could not be read.
  fn edges_of(
    &self,
    placed: &Placed,
    (format, pushed): (ObjectFormat, &Pushed<'_>),
    each: &mut impl FnMut(ObjectId, bool),
  ) -> Result<bool, Refusal> {
    if placed.kind == Kind::Blob {
      return Ok(true);
    }
    match pushed.content(self.at) {
      Some(content) => for_each_edge((placed.kind, &content), format, each),
      None => Ok(false),
    }
  }

  /// The propagation's tables charged and made: the stack and the marks, an object each.
  fn propagation_begun(&mut self, admission: &mut Admission) -> Result<(), Refusal> {
    let count = self.states.len();
    admission
      .charge_items::<(usize, usize)>(count)
      .map_err(Refusal::PastAdmission)?;
    admission
      .charge_items::<Mark>(count)
      .map_err(Refusal::PastAdmission)?;
    reserve(&mut self.stack, count)?;
    reserve(&mut self.marks, count)?;
    self.marks.resize(count, Mark::New);
    Ok(())
  }

  /// One unit of the propagation, carrying the worst state back over the edges with an explicit stack, each object
  /// finished once: the open object's next edge followed, or the object finished once its edges are; with none open,
  /// the next object not reached opened (`at` the next root); with none left, the propagation done and its tables
  /// given back.
  fn propagation_unit(&mut self) {
    let Self {
      states,
      offsets,
      targets,
      stack,
      marks,
      ..
    } = self;
    if let Some(&(node, cursor)) = stack.last() {
      let end = offsets
        .get(node.saturating_add(1))
        .copied()
        .unwrap_or(cursor);
      if cursor < end {
        if let Some(top) = stack.last_mut() {
          top.1 = cursor.saturating_add(1);
        }
        if let Some(&child) = targets.get(cursor)
          && marks.get(child) == Some(&Mark::New)
        {
          open(child, offsets, marks, stack);
        }
        return;
      }
      stack.pop();
      finish(node, states, offsets, targets, marks);
      return;
    }
    let root = self.at;
    if root >= self.states.len() {
      self.stack = Vec::new();
      self.marks = Vec::new();
      self.pass = Pass::Done;
      return;
    }
    if self.marks.get(root) == Some(&Mark::New) {
      open(root, &self.offsets, &mut self.marks, &mut self.stack);
    }
    self.at = root.saturating_add(1);
  }

  /// One unit of the walk: one object of a pass, a pass's end, or a unit of the propagation.
  fn unit<R: Repository + ?Sized>(
    &mut self,
    (request, pushed, repository): (&Request, &Pushed<'_>, &R),
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    let object = pushed.objects().get(self.at);
    match (self.pass, object) {
      (Pass::States, Some(object)) => self.judge(object, (request, pushed, repository))?,
      (Pass::States, None) => self.begin_edges(pushed.objects().len(), admission)?,
      (Pass::Edges, Some(object)) => self.keep_edges(object, request.format, pushed)?,
      (Pass::Edges, None) => {
        self.propagation_begun(admission)?;
        self.pass = Pass::Propagate;
        self.at = 0;
      }
      (Pass::Propagate, _) => self.propagation_unit(),
      (Pass::Done, _) => {}
    }
    Ok(())
  }

  /// The first pass's object: its own edges out of the pack judged, those among the pushed objects
  /// counted.
  fn judge<R: Repository + ?Sized>(
    &mut self,
    object: &Placed,
    (request, pushed, repository): (&Request, &Pushed<'_>, &R),
  ) -> Result<(), Refusal> {
    let shallow_commit = self.shallow.binary_search(&object.id).is_ok();
    let mut state = Connected::Complete;
    let mut inside = self.inside;
    let read = self.edges_of(object, (request.format, pushed), &mut |target, parent| {
      if pushed.position(&target).is_some() {
        inside = inside.saturating_add(1);
      } else if !repository.holds(&target) {
        state = state.max(if parent && shallow_commit {
          Connected::Shallow
        } else {
          Connected::Missing
        });
      }
    })?;
    self.inside = inside;
    if let Some(slot) = self.states.get_mut(self.at) {
      *slot = if read { state } else { Connected::Missing };
    }
    self.at = self.at.saturating_add(1);
    Ok(())
  }

  /// The second pass begun: its tables, for `count` objects and the edges the first counted,
  /// charged before they are made.
  fn begin_edges(&mut self, count: usize, admission: &mut Admission) -> Result<(), Refusal> {
    let bounds = count.saturating_add(1);
    admission
      .charge_items::<usize>(bounds.saturating_add(self.inside))
      .map_err(Refusal::PastAdmission)?;
    reserve(&mut self.offsets, bounds)?;
    reserve(&mut self.targets, self.inside)?;
    self.offsets.push(0);
    self.pass = Pass::Edges;
    self.at = 0;
    Ok(())
  }

  /// The second pass's object: its run of targets among the pushed objects kept.
  fn keep_edges(
    &mut self,
    object: &Placed,
    format: ObjectFormat,
    pushed: &Pushed<'_>,
  ) -> Result<(), Refusal> {
    let mut targets = core::mem::take(&mut self.targets);
    let read = self.edges_of(object, (format, pushed), &mut |target, _| {
      if let Some(at) = pushed.position(&target)
        && targets.len() < targets.capacity()
      {
        targets.push(at);
      }
    });
    self.targets = targets;
    read?;
    self.offsets.push(self.targets.len());
    self.at = self.at.saturating_add(1);
    Ok(())
  }

  /// Each command's connectivity once the walk is done ([`connectivity`]'s answer), charged a
  /// state a command.
  pub(crate) fn connected<R: Repository + ?Sized>(
    &self,
    request: &Request,
    pushed: &Pushed<'_>,
    repository: &R,
  ) -> Result<Vec<Connected>, Refusal> {
    let mut connected = Vec::new();
    reserve(&mut connected, request.commands.len())?;
    for command in &request.commands {
      let state = match pushed.position(&command.new) {
        _ if command.is_delete() => Connected::Complete,
        Some(at) => self.states.get(at).copied().unwrap_or(Connected::Missing),
        None if repository.holds(&command.new) => Connected::Complete,
        None => Connected::Missing,
      };
      connected.push(state);
    }
    Ok(connected)
  }
}

/// A sorted copy of `ids`, charged first.
fn sorted(ids: &[ObjectId], admission: &mut Admission) -> Result<Vec<ObjectId>, Refusal> {
  admission
    .charge_items::<ObjectId>(ids.len())
    .map_err(Refusal::PastAdmission)?;
  let mut sorted = Vec::new();
  reserve(&mut sorted, ids.len())?;
  sorted.extend_from_slice(ids);
  sorted.sort_unstable();
  Ok(sorted)
}

/// Hands each object an object of `kind` holding `content` names to `each`, with whether it is a
/// commit's parent: whether the object could be read. The read's transient structures, bounded by the object's own bytes, take a
/// scratch admission of their own (this module's **Bounds**).
fn for_each_edge(
  (kind, content): (Kind, &[u8]),
  format: ObjectFormat,
  each: &mut impl FnMut(ObjectId, bool),
) -> Result<bool, Refusal> {
  let mut scratch = Admission::new(u64::MAX);
  let read = match kind {
    Kind::Commit => Commit::parse(content, &mut scratch).map(|commit| {
      let tree = commit.tree(format);
      if let Some(tree) = tree {
        each(tree, false);
      }
      commit.parents(format).for_each(|parent| each(parent, true));
      tree.is_some()
    }),
    Kind::Tree => {
      Tree::parse(content, format, &mut scratch).map(|tree| tree_edges(&tree, format, each))
    }
    Kind::Tag => Tag::parse(content, &mut scratch).map(|tag| {
      let target = tag.object(format);
      if let Some(target) = target {
        each(target, false);
      }
      target.is_some()
    }),
    Kind::Blob => Ok(true),
  };
  match read {
    Ok(read) => Ok(read),
    Err(ObjectRefused {
      refusal: objects::Refusal::PastAdmission(past),
      ..
    }) => Err(Refusal::PastAdmission(past)),
    Err(_) => Ok(false),
  }
}

/// Hands each object `tree`'s entries name to `each`, but a submodule's commit, which lies in
/// another repository: whether every name could be read.
fn tree_edges(
  tree: &Tree<'_>,
  format: ObjectFormat,
  each: &mut impl FnMut(ObjectId, bool),
) -> bool {
  let mut whole = true;
  for entry in tree
    .entries()
    .iter()
    .filter(|entry| entry.kind() != Kind::Commit)
  {
    match entry.object_id(format) {
      Some(id) => each(id, false),
      None => whole = false,
    }
  }
  whole
}

/// Puts `node` on the stack, its walk at its first edge.
fn open(node: usize, offsets: &[usize], marks: &mut [Mark], stack: &mut Vec<(usize, usize)>) {
  if let Some(mark) = marks.get_mut(node) {
    *mark = Mark::Open;
  }
  let first = offsets.get(node).copied().unwrap_or(0);
  if stack.len() < stack.capacity() {
    stack.push((node, first));
  }
}

/// Makes `node`'s state the worst of its own and its targets', and marks it done.
fn finish(
  node: usize,
  states: &mut [Connected],
  offsets: &[usize],
  targets: &[usize],
  marks: &mut [Mark],
) {
  let start = offsets.get(node).copied().unwrap_or(0);
  let end = offsets
    .get(node.saturating_add(1))
    .copied()
    .unwrap_or(start);
  let worst = targets
    .get(start..end)
    .unwrap_or_default()
    .iter()
    .filter_map(|child| states.get(*child).copied())
    .max()
    .unwrap_or_default();
  if let Some(state) = states.get_mut(node) {
    *state = (*state).max(worst);
  }
  if let Some(mark) = marks.get_mut(node) {
    *mark = Mark::Done;
  }
}
