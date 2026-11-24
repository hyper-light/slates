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
//! **Bounds** (A-719: a push's working set is the slice in flight, never its history). The first pass reads every
//! pushed object once and judges its own edges out of the pack. When no object is incomplete, no tip can reach one, so
//! every pushed tip is complete and the walk ends there, its one table a state an object: the common case, every push
//! of a complete history. Otherwise the worst state is carried back from each pushed tip depth first: an object is read
//! once more as it is opened, each child's state folded into the frame of the object that opened it, so no table of
//! the pack's edges is made. The stack holds the open path's frames and the children they wait on, charged as it
//! grows, so it is bounded by the history's depth times its widest object, never by its edges; a mark an object is
//! charged once when that walk begins. The table of edges this replaced was a `usize` for every tree entry in the pack:
//! git/git's whole history asked 485,230,824 bytes for it with 300,809,433 left, and its push was refused (w-interop,
//! 2026-11-21). A read's own transient structures (A-590's header and entry lists) are a small multiple of the object's
//! bytes, which the pack's admission already charged, and are freed before the next read, so they are not charged
//! again.

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
  while !matches!(
    walk.turn(
      (request, pushed, repository),
      admission,
      unbounded(&mut stopped),
    )?,
    WalkStep::Done
  ) {}
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
  /// Each object's own edges out of the pack judged.
  States,
  /// The worst state carried back from each pushed tip, depth first.
  Propagate,
  /// Every tip's state is final.
  Done,
}

/// A frame of the propagation's stack.
#[derive(Clone, Copy, Debug)]
enum Frame {
  /// An object reached from the frame at `parent` (none for a tip), to open, or to fold into it when already done.
  Enter {
    /// The object, by position.
    node: usize,
    /// Where its parent's exit frame is on the stack.
    parent: Option<usize>,
  },
  /// An open object waiting for the children it reached: the worst state folded into it so far.
  Exit {
    /// The object, by position.
    node: usize,
    /// The worst of its own state and its finished children's.
    worst: Connected,
    /// Where its parent's exit frame is on the stack.
    parent: Option<usize>,
  },
}

/// A read push's connectivity walk stopped for a let-go window, or progressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WalkStep {
  /// More units remain within the current turn's quantum.
  More,
  /// A read met a let-go window: stop this turn now so the driver brings it back before retrying.
  Away,
  /// Every tip's connectivity is final.
  Done,
}

/// The connectivity check made a turn at a time (A-719; the module docs): every pushed object read once in pack order,
/// each turn for its quantum and at least one object; then, only when some object is incomplete, the worst state
/// carried back from each pushed tip. Its tables are charged to the admission before they are made, the stack as it
/// grows.
#[derive(Debug)]
pub(crate) struct Walk {
  /// The commits the client called shallow, sorted.
  shallow: Vec<ObjectId>,
  /// Each pushed object's state, in pack order: its own until the propagation, then final for every object it reached.
  states: Vec<Connected>,
  /// Whether the first pass found an object incomplete of its own.
  incomplete: bool,
  /// The pass under way.
  pass: Pass,
  /// The next object of the first pass, or the next command whose tip the propagation starts from.
  at: usize,
  /// The propagation's stack: the open path's exit frames and the children they wait on.
  stack: Vec<Frame>,
  /// Where each object's propagation stands; empty unless the propagation runs.
  marks: Vec<Mark>,
  /// Where the walk last stopped for a let-go window `(pass, node, offset)` so a failed bring-back
  /// never spins indefinitely.
  stopped: Option<(Pass, usize, u64)>,
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
      incomplete: false,
      pass: Pass::States,
      at: 0,
      stack: Vec::new(),
      marks: Vec::new(),
      stopped: None,
    })
  }

  /// One turn of the walk over `request`'s `pushed` objects and `repository`, for `quantum` and at
  /// least one unit of work: whether the walk is done, needs another turn, or paused for a let-go window.
  pub(crate) fn turn<R: Repository + ?Sized>(
    &mut self,
    (request, pushed, repository): (&Request, &Pushed<'_>, &R),
    admission: &mut Admission,
    quantum: Quantum<'_>,
  ) -> Result<WalkStep, Refusal> {
    let Quantum { clock, ns } = quantum;
    let begun = clock.now_ns();
    while self.pass != Pass::Done {
      if self.unit((request, pushed, repository), admission)? == WalkStep::Away {
        return Ok(WalkStep::Away);
      }
      if clock.now_ns().saturating_sub(begun) >= ns {
        break;
      }
    }
    Ok(if self.pass == Pass::Done {
      WalkStep::Done
    } else {
      WalkStep::More
    })
  }

  /// One unit of the walk: one object of the first pass, its end, or a frame of the propagation.
  fn unit<R: Repository + ?Sized>(
    &mut self,
    (request, pushed, repository): (&Request, &Pushed<'_>, &R),
    admission: &mut Admission,
  ) -> Result<WalkStep, Refusal> {
    match self.pass {
      Pass::States => match pushed.objects().get(self.at) {
        Some(object) => self.judge(object, (request, pushed, repository)),
        None => {
          self.propagation_begun(admission)?;
          Ok(WalkStep::More)
        }
      },
      Pass::Propagate => self.propagation_unit((request, pushed), admission),
      Pass::Done => Ok(WalkStep::Done),
    }
  }

  /// The first pass's object: its own edges out of the pack judged.
  fn judge<R: Repository + ?Sized>(
    &mut self,
    object: &Placed,
    (request, pushed, repository): (&Request, &Pushed<'_>, &R),
  ) -> Result<WalkStep, Refusal> {
    let shallow_commit = self.shallow.binary_search(&object.id).is_ok();
    let mut state = Connected::Complete;
    let read = edges_of(
      (self.at, object),
      (request.format, pushed),
      &mut |target, parent| {
        if pushed.position(&target).is_none() && !repository.holds(&target) {
          state = state.max(if parent && shallow_commit {
            Connected::Shallow
          } else {
            Connected::Missing
          });
        }
      },
    )?;
    if !read
      && let Some(away) = pushed.away()
      && self.stopped != Some((Pass::States, self.at, away))
    {
      self.stopped = Some((Pass::States, self.at, away));
      return Ok(WalkStep::Away);
    }
    self.stopped = None;
    let state = if read { state } else { Connected::Missing };
    self.incomplete |= state != Connected::Complete;
    if let Some(slot) = self.states.get_mut(self.at) {
      *slot = state;
    }
    self.at = self.at.saturating_add(1);
    Ok(WalkStep::More)
  }

  /// The first pass ended: done when no object is incomplete of its own (the module docs), else the propagation begun
  /// from the first command's tip, its marks charged before they are made.
  fn propagation_begun(&mut self, admission: &mut Admission) -> Result<(), Refusal> {
    self.at = 0;
    if !self.incomplete {
      self.pass = Pass::Done;
      return Ok(());
    }
    let count = self.states.len();
    admission
      .charge_items::<Mark>(count)
      .map_err(Refusal::PastAdmission)?;
    reserve(&mut self.marks, count)?;
    self.marks.resize(count, Mark::New);
    self.pass = Pass::Propagate;
    Ok(())
  }

  /// One unit of the propagation: the top frame handled; with none, the next command's pushed tip entered; with no
  /// command left, the propagation done and its tables given back.
  fn propagation_unit(
    &mut self,
    (request, pushed): (&Request, &Pushed<'_>),
    admission: &mut Admission,
  ) -> Result<WalkStep, Refusal> {
    match self.stack.pop() {
      Some(Frame::Enter { node, parent }) => {
        self.enter(node, parent, (request.format, pushed), admission)
      }
      Some(Frame::Exit {
        node,
        worst,
        parent,
      }) => {
        let state = match self.states.get_mut(node) {
          Some(state) => {
            *state = (*state).max(worst);
            *state
          }
          None => worst,
        };
        if let Some(mark) = self.marks.get_mut(node) {
          *mark = Mark::Done;
        }
        self.fold(parent, state);
        Ok(WalkStep::More)
      }
      None => {
        let Some(command) = request.commands.get(self.at) else {
          self.stack = Vec::new();
          self.marks = Vec::new();
          self.pass = Pass::Done;
          return Ok(WalkStep::Done);
        };
        self.at = self.at.saturating_add(1);
        match pushed.position(&command.new) {
          Some(tip) if !command.is_delete() => {
            self.push(
              Frame::Enter {
                node: tip,
                parent: None,
              },
              admission,
            )?;
            Ok(WalkStep::More)
          }
          _ => Ok(WalkStep::More),
        }
      }
    }
  }

  /// `node`, reached from the exit frame at `parent`: folded into it when already done; else opened, its exit frame
  /// pushed and each child not yet done entered after it, a done child folded at once.
  fn enter(
    &mut self,
    node: usize,
    parent: Option<usize>,
    (format, pushed): (ObjectFormat, &Pushed<'_>),
    admission: &mut Admission,
  ) -> Result<WalkStep, Refusal> {
    match self.marks.get(node) {
      Some(Mark::New) => {}
      Some(Mark::Done) => {
        let state = self.states.get(node).copied().unwrap_or(Connected::Missing);
        self.fold(parent, state);
        return Ok(WalkStep::More);
      }
      // An object open on the path below cannot be reached again, its names being its children's hashes; nothing
      // to fold.
      Some(Mark::Open) | None => return Ok(WalkStep::More),
    }
    if let Some(mark) = self.marks.get_mut(node) {
      *mark = Mark::Open;
    }
    let own = self.states.get(node).copied().unwrap_or(Connected::Missing);
    let exit = self.stack.len();
    self.push(
      Frame::Exit {
        node,
        worst: own,
        parent,
      },
      admission,
    )?;
    let Some(object) = pushed.objects().get(node) else {
      return Ok(WalkStep::More);
    };
    let mut worst = own;
    let mut refused = None;
    let Self {
      stack,
      marks,
      states,
      ..
    } = self;
    let read = edges_of((node, object), (format, pushed), &mut |target, _| {
      let Some(child) = pushed.position(&target) else {
        return;
      };
      match marks.get(child) {
        Some(Mark::Done) => {
          worst = worst.max(states.get(child).copied().unwrap_or(Connected::Missing));
        }
        Some(Mark::New) if refused.is_none() => {
          if let Err(past) = grown(stack, admission) {
            refused = Some(past);
            return;
          }
          stack.push(Frame::Enter {
            node: child,
            parent: Some(exit),
          });
        }
        _ => {}
      }
    })?;
    if !read
      && let Some(away) = pushed.away()
      && self.stopped != Some((Pass::Propagate, node, away))
    {
      self.stack.truncate(exit);
      if let Some(mark) = self.marks.get_mut(node) {
        *mark = Mark::New;
      }
      self.push(Frame::Enter { node, parent }, admission)?;
      self.stopped = Some((Pass::Propagate, node, away));
      return Ok(WalkStep::Away);
    }
    self.stopped = None;
    if let Some(Frame::Exit { worst: folded, .. }) = self.stack.get_mut(exit) {
      *folded = (*folded).max(worst);
    }
    refused.map_or(Ok(WalkStep::More), Err)
  }

  /// `state` folded into the exit frame at `parent`, if any.
  fn fold(&mut self, parent: Option<usize>, state: Connected) {
    if let Some(Frame::Exit { worst, .. }) = parent.and_then(|at| self.stack.get_mut(at)) {
      *worst = (*worst).max(state);
    }
  }

  /// `frame` pushed on the stack, its growth charged first.
  fn push(&mut self, frame: Frame, admission: &mut Admission) -> Result<(), Refusal> {
    grown(&mut self.stack, admission)?;
    self.stack.push(frame);
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

/// Room for one more frame on `stack`: when it is full, its capacity doubled (one frame at least), the growth charged
/// to `admission` before it is made.
fn grown(stack: &mut Vec<Frame>, admission: &mut Admission) -> Result<(), Refusal> {
  if stack.len() < stack.capacity() {
    return Ok(());
  }
  let more = stack.capacity().max(1);
  admission
    .charge_items::<Frame>(more)
    .map_err(Refusal::PastAdmission)?;
  reserve(stack, more)
}

/// The edges of the object at position `at`, as `pushed` lends its bytes, handed to `each`: whether it could be read.
/// A blob names nothing, so its bytes are not read; an object whose bytes are not lent could not be read.
fn edges_of(
  (at, placed): (usize, &Placed),
  (format, pushed): (ObjectFormat, &Pushed<'_>),
  each: &mut impl FnMut(ObjectId, bool),
) -> Result<bool, Refusal> {
  if placed.kind == Kind::Blob {
    return Ok(true);
  }
  match pushed.content(at) {
    Some(content) => for_each_edge((placed.kind, &content), format, each),
    None => Ok(false),
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
