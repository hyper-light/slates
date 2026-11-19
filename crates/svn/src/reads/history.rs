//! FSFS's node history on the index (§4.17.4 item 2 and "4. Reads": log and blame read the history lists; A-613 part
//! 2): `svn_fs_node_history2` and `svn_fs_history_prev2` as `tree.c` answers them (`fs_node_history`,
//! `fs_history_prev`, `history_prev`; Subversion 1.15.0), the walk `log` and `get-file-revs` take back through a node's
//! line of history, across the copies it came through.
//!
//! A history is a location, a path in a revision, with whether it is reported ("interesting") and, after a copy, the
//! copy's source to go on from. One step back, as `history_prev` takes it:
//! - From a location given a copy's source, the source is the place to look, unreported, unless copies are not to be
//!   crossed: then the history ends.
//! - The node at the place: when the place's revision is the node's created revision, the node itself is reported if it
//!   was not yet; if it was, the walk moves to its predecessor (none: the history ends).
//! - Then the youngest copy root on the place's path (`find_youngest_copyroot`, the crate's one walk of copy roots in
//!   `crate::report::drive::history`): when it is younger than the node's created revision and the place is at or
//!   beneath that copy's destination, the next location is the place in the copy's revision, carrying the copy's
//!   source joined with the rest of the path; reported unless it is the location just reported, which is then passed
//!   over ("retry").
//! - Otherwise the next location is the node's created path in its created revision, reported.
//!
//! The root changes in every revision and is never copied, so its history is every revision down to 0
//! (`fs_history_prev`'s special case). `fs_history_prev` steps until a location is reported, which takes at most two
//! steps ([`PREVIOUS_STEPS`]): a passed-over location is a copy's, and the step after it starts at the copy's source,
//! where every case reports. `tree.c`'s shortcut through linear history (`next_copy`, `current_id`) answers as the
//! steps do, so it is not kept.

use super::{Failure, ROOT, Reads, Repository, SEPARATOR};
use crate::index::Revision;
use crate::report::drive::history as copies;

/// The most steps one `fs_history_prev` takes before a location is reported.
/// Derived: a location passed over is a copy's retry, and the step after it starts at the copy's source, where every
/// case of `history_prev` reports; so the second step reports or ends the history.
const PREVIOUS_STEPS: usize = 2;

/// One location of a node's history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct History {
  /// The path.
  pub(super) path: String,
  /// The revision.
  pub(super) revision: Revision,
  /// Whether it is reported (`is_interesting`).
  interesting: bool,
  /// After a copy, where its source stands: the place the next step starts from (`path_hint`, `rev_hint`).
  hint: Option<(String, Revision)>,
}

impl<R: Repository + ?Sized> Reads<'_, R> {
  /// `svn_fs_node_history2`: the history of the node at `path` in `revision`, not yet reported; refused when the path
  /// is absent there ("File not found").
  pub(super) fn node_history(&self, path: &str, revision: Revision) -> Result<History, Failure> {
    if self.node(path, revision)?.is_none() {
      return Err(Failure::not_found(path, revision));
    }
    Ok(History {
      path: path.to_owned(),
      revision,
      interesting: false,
      hint: None,
    })
  }

  /// `svn_fs_history_prev2`: the history's previous reported location, crossing copies when `cross_copies`; `None`
  /// when the history ends.
  pub(super) fn history_prev(
    &self,
    history: &History,
    cross_copies: bool,
  ) -> Result<Option<History>, Failure> {
    if history.path == ROOT {
      return Ok(root_prev(history));
    }
    let mut current = history.clone();
    for _ in 0..PREVIOUS_STEPS {
      match self.history_step(&current, cross_copies)? {
        Some(previous) if previous.interesting => return Ok(Some(previous)),
        Some(previous) => current = previous,
        None => return Ok(None),
      }
    }
    Err(Failure::corrupt())
  }

  /// `history_prev`: one step back from a location.
  fn history_step(&self, history: &History, cross_copies: bool) -> Result<Option<History>, Failure> {
    let (path, revision, reported) = match &history.hint {
      Some(_) if !cross_copies => return Ok(None),
      Some((path, revision)) => (path.as_str(), *revision, false),
      None => (history.path.as_str(), history.revision, history.interesting),
    };
    let node = self
      .node(path, revision)?
      .ok_or_else(|| Failure::not_found(path, revision))?;
    let (mut commit_path, mut commit_revision) = (node.created_path(), node.created_revision());
    if revision == commit_revision {
      if !reported {
        return Ok(Some(reported_at(commit_path, commit_revision)));
      }
      let Some(predecessor) = node.predecessor() else {
        return Ok(None);
      };
      let predecessor = self
        .index
        .node_revision(predecessor)
        .map_err(|refusal| Failure::of_index(&refusal))?;
      (commit_path, commit_revision) = (predecessor.created_path(), predecessor.created_revision());
    }
    if let Some((copied_in, source)) = self.copy_crossed(path, revision, commit_revision)? {
      let retry = copied_in == revision && reported;
      return Ok(Some(History {
        path: path.to_owned(),
        revision: copied_in,
        interesting: !retry,
        hint: Some(source),
      }));
    }
    Ok(Some(reported_at(commit_path, commit_revision)))
  }

  /// The copy `history_prev` crosses at `path` in `revision`: the youngest copy root on the path, when it is younger
  /// than `commit_revision` and the path is its destination or beneath it, as its revision and its source joined with
  /// the rest of the path; `None` when there is none, or when the copy root records no source.
  fn copy_crossed(
    &self,
    path: &str,
    revision: Revision,
    commit_revision: Revision,
  ) -> Result<Option<(Revision, (String, Revision))>, Failure> {
    let youngest = copies::youngest_copy_root(self.index, path, revision)
      .map_err(|refusal| Failure::of_index(&refusal))?;
    let Some((copied_in, copy_path)) = youngest else {
      return Ok(None);
    };
    if copied_in <= commit_revision {
      return Ok(None);
    }
    let copy_root = self
      .node(&copy_path, copied_in)?
      .ok_or_else(|| Failure::not_found(&copy_path, copied_in))?;
    let Some(remainder) = skip_ancestor(copy_root.created_path(), path) else {
      return Ok(None);
    };
    let Some((source, source_revision)) = copy_root.copy_from() else {
      return Ok(None);
    };
    Ok(Some((copied_in, (join(source, remainder), source_revision))))
  }
}

/// A reported location with no copy to cross.
fn reported_at(path: &str, revision: Revision) -> History {
  History {
    path: path.to_owned(),
    revision,
    interesting: true,
    hint: None,
  }
}

/// `fs_history_prev`'s special case for the root, which changes in every revision and is never copied: the same
/// revision when it is not yet reported, else the one before, down to 0.
fn root_prev(history: &History) -> Option<History> {
  let revision = if history.interesting {
    history.revision.checked_sub(1)?
  } else {
    history.revision
  };
  Some(reported_at(ROOT, revision))
}

/// `svn_fspath__skip_ancestor`: what `child` adds to `parent` when it is `parent` or beneath it (empty for `parent`
/// itself); `None` otherwise.
pub(super) fn skip_ancestor<'child>(parent: &str, child: &'child str) -> Option<&'child str> {
  if parent == ROOT {
    return child.strip_prefix(SEPARATOR);
  }
  if child == parent {
    return Some("");
  }
  child.strip_prefix(parent)?.strip_prefix(SEPARATOR)
}

/// `svn_fspath__join`: `base` with a relative path appended, `base` itself for an empty one.
pub(super) fn join(base: &str, relative: &str) -> String {
  if relative.is_empty() {
    base.to_owned()
  } else if base == ROOT {
    format!("{ROOT}{relative}")
  } else {
    format!("{base}{SEPARATOR}{relative}")
  }
}
