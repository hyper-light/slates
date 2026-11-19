//! The connectivity check (A-620 part 1; §4.17.2 "A push", item 5: "every object a new commit, tree or tag names is
//! either in the push or in the repository's index", in time "proportional to the new objects", walking no history;
//! git's `check_connected` and `set_connectivity_errors`, v2.50.1, which ask `rev-list --objects` for the new tips not
//! reachable from any ref).
//!
//! The walk is of the pushed objects alone. An object the repository holds ends it, since nothing enters the repository
//! before its push has passed this check. Each pushed object names others: a commit its tree and parents, a tree its
//! entries (a submodule's commit excepted, as git's walk excepts it: it lies in another repository), a tag its object.
//! A name that is neither pushed nor held makes the object, and every pushed object that reaches it, incomplete:
//! `Shallow` when it is a parent of a commit the client called shallow (the push would make the repository shallow),
//! `Missing` otherwise. An object that cannot be read is `Missing`, since what it names cannot be known.
//!
//! **Bounds.** Every object is read twice, once to count the edges among the pushed objects and once to keep them, so
//! the tables are charged exactly before they are made; each read's own transient structures are held to what the
//! admission has left, and never charged. The worst state is carried back over the edges with an explicit stack, each
//! object finished once.

use super::{Pushed, Refusal, Repository, Request, reserve};
use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::{self, ObjectRefused, commit::Commit, tag::Tag, tree::Tree};
use crate::unpack::Object;

/// Whether a command's new tip is connected; worse states order later.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Connected {
  /// Everything it reaches is pushed or held.
  #[default]
  Complete,
  /// It reaches a commit the client called shallow whose parent nothing holds (git's "shallow update not allowed").
  Shallow,
  /// It reaches an object nothing holds, or one that cannot be read (git's "missing necessary objects").
  Missing,
}

/// Where a walk of one pushed object stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
  /// Not reached.
  New,
  /// On the stack.
  Open,
  /// Its state is final.
  Done,
}

/// Each command's connectivity, in order: a delete is `Complete`; a new tip pushed has its walk's state; a new tip not
/// pushed is `Complete` when the repository holds it and `Missing` otherwise.
pub fn connectivity<R: Repository + ?Sized>(
  request: &Request,
  pushed: &Pushed<'_>,
  repository: &R,
  admission: &mut Admission,
) -> Result<Vec<Connected>, Refusal> {
  let states = states(request, pushed, repository, admission)?;
  let mut connected = Vec::new();
  reserve(&mut connected, request.commands.len())?;
  for command in &request.commands {
    let state = match pushed.position(&command.new) {
      _ if command.is_delete() => Connected::Complete,
      Some(at) => states.get(at).copied().unwrap_or(Connected::Missing),
      None if repository.holds(&command.new) => Connected::Complete,
      None => Connected::Missing,
    };
    connected.push(state);
  }
  Ok(connected)
}

/// Each pushed object's state: its own edges out of the pack judged, then the worst of everything it reaches.
fn states<R: Repository + ?Sized>(
  request: &Request,
  pushed: &Pushed<'_>,
  repository: &R,
  admission: &mut Admission,
) -> Result<Vec<Connected>, Refusal> {
  let objects = pushed.objects();
  let count = objects.len();
  admission
    .charge_items::<Connected>(count)
    .map_err(Refusal::PastAdmission)?;
  let mut states = Vec::new();
  reserve(&mut states, count)?;
  states.resize(count, Connected::Complete);
  let shallow = sorted(&request.shallow, admission)?;
  let mut inside = 0usize;
  for (at, object) in objects.iter().enumerate() {
    let shallow_commit = shallow.binary_search(&object.id).is_ok();
    let mut state = Connected::Complete;
    let read = for_each_edge(object, request.format, admission, &mut |target, parent| {
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
    if let Some(slot) = states.get_mut(at) {
      *slot = if read { state } else { Connected::Missing };
    }
  }
  let (offsets, targets) = edges(pushed, request.format, inside, admission)?;
  propagate(&mut states, &offsets, &targets, admission)?;
  Ok(states)
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

/// The edges among the pushed objects, `inside` of them, as each object's run of targets: the runs' bounds (one more
/// than the objects) and the targets, by position, charged before they are made.
fn edges(
  pushed: &Pushed<'_>,
  format: ObjectFormat,
  inside: usize,
  admission: &mut Admission,
) -> Result<(Vec<usize>, Vec<usize>), Refusal> {
  let objects = pushed.objects();
  let bounds = objects.len().saturating_add(1);
  admission
    .charge_items::<usize>(bounds.saturating_add(inside))
    .map_err(Refusal::PastAdmission)?;
  let mut offsets = Vec::new();
  reserve(&mut offsets, bounds)?;
  let mut targets = Vec::new();
  reserve(&mut targets, inside)?;
  offsets.push(0);
  for object in objects {
    for_each_edge(object, format, admission, &mut |target, _| {
      if let Some(at) = pushed.position(&target)
        && targets.len() < targets.capacity()
      {
        targets.push(at);
      }
    })?;
    offsets.push(targets.len());
  }
  Ok((offsets, targets))
}

/// Hands each object `object` names to `each`, with whether it is a commit's parent: whether the object could be read.
/// Each read holds its transient structures to what `admission` has left, without charging them; a read past that is
/// refused as the admission's.
fn for_each_edge(
  object: &Object,
  format: ObjectFormat,
  admission: &Admission,
  each: &mut impl FnMut(ObjectId, bool),
) -> Result<bool, Refusal> {
  let mut scratch = Admission::new(admission.left());
  let read = match object.kind {
    Kind::Commit => Commit::parse(&object.content, &mut scratch).map(|commit| {
      let tree = commit.tree(format);
      if let Some(tree) = tree {
        each(tree, false);
      }
      commit.parents(format).for_each(|parent| each(parent, true));
      tree.is_some()
    }),
    Kind::Tree => {
      Tree::parse(&object.content, format, &mut scratch).map(|tree| tree_edges(&tree, format, each))
    }
    Kind::Tag => Tag::parse(&object.content, &mut scratch).map(|tag| {
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

/// Hands each object `tree`'s entries name to `each`, but a submodule's commit, which lies in another repository:
/// whether every name could be read.
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

/// Makes each object's state the worst of its own and of every object it reaches, by an explicit stack of objects and
/// the next edge of each, every object finished once. An edge back to an object on the stack, which object names
/// cannot make, is passed over.
fn propagate(
  states: &mut [Connected],
  offsets: &[usize],
  targets: &[usize],
  admission: &mut Admission,
) -> Result<(), Refusal> {
  let count = states.len();
  admission
    .charge_items::<(usize, usize)>(count)
    .map_err(Refusal::PastAdmission)?;
  admission
    .charge_items::<Mark>(count)
    .map_err(Refusal::PastAdmission)?;
  let mut stack: Vec<(usize, usize)> = Vec::new();
  reserve(&mut stack, count)?;
  let mut marks = Vec::new();
  reserve(&mut marks, count)?;
  marks.resize(count, Mark::New);
  for root in 0..count {
    if marks.get(root) != Some(&Mark::New) {
      continue;
    }
    open(root, offsets, &mut marks, &mut stack);
    while let Some(&(node, cursor)) = stack.last() {
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
          open(child, offsets, &mut marks, &mut stack);
        }
        continue;
      }
      stack.pop();
      finish(node, states, offsets, targets, &mut marks);
    }
  }
  Ok(())
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
