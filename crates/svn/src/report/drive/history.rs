//! The two questions of a path's history the drive asks (A-616, part 2): FSFS's closest copy and
//! the revision a path was deleted in, answered over the index (A-553) as Subversion answers them
//! (`libsvn_fs_fs/tree.c` `fs_closest_copy` and `find_youngest_copyroot`; `libsvn_repos/rev_hunt.c`
//! `svn_repos_deleted_rev`; Subversion 1.15.0).
//!
//! A path out of Subversion's canonical form names nothing here, as FSFS's `open_path` finds no
//! entry named `..`: the report's paths are the client's.
//!
//! It is the crate's one walk of copy roots: the read commands (`reads`: `get-locations`,
//! `get-location-segments`, `get-deleted-rev`, and FSFS's node history under `log`, which reads
//! the youngest copy root itself) call it too, rather than keep a second (main's decision,
//! 2026-11-19).

use crate::index::{Index, IndexRefusal, Node, Revision};

/// The root's path, where every path's chain of directories starts.
/// Format: `svn_fspath`'s root.
const ROOT: &str = "/";

/// The most halvings a binary search over revisions makes: one a bit of a revision number, and one
/// more for the last comparison. A search that has not ended by then answers none.
/// Format: a revision is a `u64`, and each step halves the range searched.
const MOST_HALVINGS: u32 = u64::BITS + 1;

/// The node at `path` in `revision`; none when the path is absent there or out of canonical form.
///
/// # Errors
/// The index's refusal of the revision or of its own rows.
pub(crate) fn present<'index>(
  index: &'index Index,
  path: &str,
  revision: Revision,
) -> Result<Option<Node<'index>>, IndexRefusal> {
  match index.node(path, revision) {
    Ok(node) => Ok(node),
    Err(IndexRefusal::PathSyntax { .. }) => Ok(None),
    Err(refusal) => Err(refusal),
  }
}

/// `fs_closest_copy`: the revision and the path of the copy that put the node at `path` in
/// `revision` where it is, the innermost one over the path; none when no copy did, when the node is
/// on another line of history than the copy's, or when it was made new beneath the copy in the
/// copy's own revision.
///
/// # Errors
/// The index's refusal of the revision or of its own rows.
pub(crate) fn closest_copy(
  index: &Index,
  path: &str,
  revision: Revision,
) -> Result<Option<(Revision, String)>, IndexRefusal> {
  let Some(node) = present(index, path, revision)? else {
    return Ok(None);
  };
  let Some((copied_in, copy_path)) = youngest_copy_root(index, path, revision)? else {
    return Ok(None);
  };
  let Some(copied) = present(index, path, copied_in)? else {
    return Ok(None);
  };
  if copied.node_id() != node.node_id() {
    return Ok(None);
  }
  if copied.created_revision() == copied_in && copied.predecessor().is_none() {
    return Ok(None);
  }
  Ok(Some((copied_in, copy_path)))
}

/// `find_youngest_copyroot`: the youngest copy root among the nodes from the root down to `path` in
/// `revision`, a child's preferred over a parent's of the same revision; none when it is revision
/// 0's, which no copy made.
pub(crate) fn youngest_copy_root(
  index: &Index,
  path: &str,
  revision: Revision,
) -> Result<Option<(Revision, String)>, IndexRefusal> {
  let mut youngest: Option<(Revision, String)> = None;
  for place in chain(path) {
    let Some(node) = present(index, place, revision)? else {
      continue;
    };
    let root = index.node_revision(node.copy_root())?;
    if youngest
      .as_ref()
      .is_none_or(|(kept, _)| root.created_revision() >= *kept)
    {
      youngest = Some((root.created_revision(), root.created_path().to_owned()));
    }
  }
  Ok(youngest.filter(|(copied_in, _)| *copied_in != 0))
}

/// The paths from the root down to `path`: `/`, `/a`, `/a/b` for `/a/b`.
fn chain(path: &str) -> impl Iterator<Item = &str> {
  let inner = path
    .match_indices('/')
    .skip(1)
    .filter_map(|(at, _)| path.get(..at));
  std::iter::once(ROOT)
    .chain(inner)
    .chain((path != ROOT).then_some(path))
}

/// `svn_repos_deleted_rev`: the first revision after `start` and at most `end` in which the node at
/// `path` in `start` was deleted, replaced, or replaced by a copy of itself; none when the path is
/// absent in `start`, or the node is there in `end` uncopied since `start`.
///
/// # Errors
/// The index's refusal of a revision or of its own rows.
pub(crate) fn deleted_revision(
  index: &Index,
  path: &str,
  start: Revision,
  end: Revision,
) -> Result<Option<Revision>, IndexRefusal> {
  let (mut start, mut end) = (start.min(end), start.max(end));
  let Some(first) = present(index, path, start)? else {
    return Ok(None);
  };
  if let Some(last) = present(index, path, end)?
    && last.node_id() == first.node_id()
    && !copied_after(index, path, end, start)?
  {
    return Ok(None);
  }
  let mut middle = midpoint(start, end);
  for _ in 0..MOST_HALVINGS {
    let held = present(index, path, middle)?;
    let lower = match held {
      None => true,
      Some(node) => node.node_id() != first.node_id() || copied_after(index, path, middle, start)?,
    };
    if lower {
      end = middle;
      middle = midpoint(start, middle);
    } else if end.checked_sub(middle) == Some(1) {
      return Ok(Some(end));
    } else {
      start = middle;
      middle = midpoint(start, end);
    }
  }
  Ok(None)
}

/// Whether the closest copy over `path` in `revision` was made after `start`.
fn copied_after(
  index: &Index,
  path: &str,
  revision: Revision,
  start: Revision,
) -> Result<bool, IndexRefusal> {
  Ok(closest_copy(index, path, revision)?.is_some_and(|(copied_in, _)| copied_in > start))
}

/// The revision halfway from `low` to `high`, rounded down, as `(start + end) / 2` makes it.
fn midpoint(low: Revision, high: Revision) -> Revision {
  low.saturating_add(high.saturating_sub(low) / 2)
}
