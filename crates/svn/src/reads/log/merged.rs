//! `log`'s merged revisions (§4.17.4 "4. Reads": `include-merged-revisions` reads the mergeinfo changes the revision's
//! rows name; item 8; A-613 part 2): what `svn_repos_get_logs5` computes to find a revision's merged revisions, as
//! `log.c` computes it (Subversion 1.15.0), over A-605's mergeinfo and the index:
//! - The log's own history as mergeinfo (`get_paths_history_as_mergeinfo`): each path's location segments over the
//!   range, each a source and the revisions before it, so a merged revision whose every changed path lies in the log's
//!   own history is passed over (`interesting_merge`).
//! - A revision's mergeinfo changes (`fs_mergeinfo_changed`): each path of the revision whose `svn:mergeinfo` changed,
//!   compared with its value at its base (the revision before, or the copy's source; for a value set or removed, the
//!   inherited value standing in for the missing one), as the mergeinfo it deleted and added; a value that does not
//!   parse makes the revision one without changes (issue #3896).
//! - The changes to the log's paths (`get_combined_mergeinfo_changes`): a path's inherited mergeinfo compared with its
//!   previous location's, unless the change came by a copy whose inherited mergeinfo is the same (issue #4022); then
//!   every changed path at or beneath a log path. The paths are each history's next location (`do_logs` builds them
//!   after the histories have moved on).
//! - The merged revisions, as runs of revisions and the sources merged in them (`combine_mergeinfo_path_lists`), each
//!   walked by a nested `do_logs`, reverse merges marked.
//! - The search's bookkeeping (`reduce_search`, `store_search`): what nested walks have covered, so a source and range
//!   already searched is not searched again.
//!
//! Where Subversion's result depends on its hash tables' seeded order it is nondeterministic, and this port takes the
//! order the C code intends: `combine_mergeinfo_path_lists` removes the first list of its array (`array_pop_front`)
//! where it means the list whose last range it used up, which differs only when two sources share their first range
//! and the one sorted first holds more ranges; here the list used up is removed. Its sort of runs with the same range
//! is stable here, where `qsort` leaves them in an order of its own.

use crate::index::{Action, PropertyList, Revision};
use crate::mergeinfo::{
  Compare, MergeRange, Mergeinfo, MergeinfoDelta, MergeinfoInheritance, RangeList, SourcePath,
  mergeinfo_as_held, mergeinfo_at,
};
use crate::reads::history::skip_ancestor;
use crate::reads::segments::Segment;
use crate::reads::{Failure, Reads, Repository, SVN_ERR_FS_NOT_DIRECTORY, SVN_ERR_FS_NOT_FOUND};
use std::collections::BTreeMap;

/// The mergeinfo changes one revision made to the log's paths.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct MergeinfoChanges {
  /// The mergeinfo the revision added: merges.
  pub(super) added: Mergeinfo,
  /// The mergeinfo it deleted: reverse merges.
  pub(super) deleted: Mergeinfo,
}

impl MergeinfoChanges {
  /// Whether the revision merged or reverse merged anything: its entry has children (`has_children`).
  pub(super) fn any(&self) -> bool {
    !self.added.is_empty() || !self.deleted.is_empty()
  }

  /// The changes with another delta's merged in (`svn_mergeinfo_merge2`, twice).
  fn absorb(&mut self, delta: &MergeinfoDelta) {
    self.added = self.added.merge(&delta.added);
    self.deleted = self.deleted.merge(&delta.deleted);
  }
}

/// A run of merged revisions and the sources it was merged from (`struct path_list_range`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PathListRange {
  /// The sources, absolute paths.
  pub(super) paths: Vec<String>,
  /// The run's first revision.
  pub(super) start: Revision,
  /// The run's last revision.
  pub(super) end: Revision,
  /// Whether the run was reverse merged.
  pub(super) reverse: bool,
}

/// Each changed path's mergeinfo deltas in one revision (`fs_mergeinfo_changed`'s two catalogs, kept together).
type Catalog = BTreeMap<String, MergeinfoDelta>;

/// Why a revision's mergeinfo changes are none: a value that does not parse (issue #3896), or a refusal.
enum Unread {
  /// A stored value did not parse: the revision is one without changes.
  Unparsed,
  /// A refusal the log ends with.
  Refused(Failure),
}

impl From<Failure> for Unread {
  fn from(failure: Failure) -> Self {
    Self::Refused(failure)
  }
}

impl<R: Repository + ?Sized> Reads<'_, R> {
  /// `get_paths_history_as_mergeinfo`: each path's location segments from the range's youngest revision down to its
  /// oldest, as mergeinfo: each segment's path the source of the revisions after its start through its end. As
  /// `svn_repos_node_location_segments` walks them with the session's rules: a path it may not read refused
  /// (`check_readability`), a segment it may not read ending that path's walk.
  pub(super) fn history_as_mergeinfo(
    &self,
    paths: &[String],
    oldest: Revision,
    youngest: Revision,
  ) -> Result<Mergeinfo, Failure> {
    let mut history = Mergeinfo::default();
    for path in paths {
      if !self.readable(path) {
        return Err(Failure::unreadable_path());
      }
      let mut segments = Vec::new();
      let mut next = Some((path.clone(), youngest));
      while let Some((place, revision)) = next.take().filter(|(_, revision)| *revision >= oldest) {
        let turn = self.segment_turn(&place, revision, youngest, oldest)?;
        segments.extend(turn.sent);
        next = turn.next;
      }
      history = history.merge(&from_segments(&segments));
    }
    Ok(history)
  }

  /// `get_combined_mergeinfo_changes`: the mergeinfo `revision` added and deleted for `paths`, each path's inherited
  /// mergeinfo against its previous location's, then every changed path at or beneath one of them.
  pub(super) fn mergeinfo_changes(
    &self,
    paths: &[String],
    revision: Revision,
  ) -> Result<MergeinfoChanges, Failure> {
    let mut changes = MergeinfoChanges::default();
    if revision == 0 || paths.is_empty() {
      return Ok(changes);
    }
    let catalog = match self.catalog(revision) {
      Ok(catalog) => catalog,
      Err(Unread::Unparsed) => return Ok(changes),
      Err(Unread::Refused(failure)) => return Err(failure),
    };
    if catalog.is_empty() {
      return Ok(changes);
    }
    for path in paths {
      if catalog.contains_key(path) {
        continue;
      }
      if let Some(delta) = self.inherited_change(path, revision)? {
        changes.absorb(&delta);
      }
    }
    for (changed, delta) in &catalog {
      if paths
        .iter()
        .any(|path| skip_ancestor(path, changed).is_some())
      {
        changes.absorb(delta);
      }
    }
    Ok(changes)
  }

  /// `get_combined_mergeinfo_changes`' check of one path: its inherited mergeinfo in `revision` against its previous
  /// location's (the copy's source when a copy made it in `revision`, else itself the revision before), passed over
  /// when either is missing, when neither has any, and when the inherited part is the same (issue #4022).
  fn inherited_change(
    &self,
    path: &str,
    revision: Revision,
  ) -> Result<Option<MergeinfoDelta>, Failure> {
    let signed = i64::try_from(revision).map_err(|_| Failure::corrupt())?;
    let previous = match self.previous_location(path, signed) {
      Ok(previous) => previous,
      Err(failure) if passed_over(&failure) => return Ok(None),
      Err(failure) => return Err(failure),
    };
    let (previous_path, previous_revision) = match previous {
      Some((appeared, previous_path, previous_revision)) if appeared == signed => {
        let previous_revision =
          Revision::try_from(previous_revision).map_err(|_| Failure::corrupt())?;
        (previous_path, previous_revision)
      }
      _ => (path.to_owned(), revision.saturating_sub(1)),
    };
    let before = match self.query_mergeinfo(&previous_path, previous_revision, false) {
      Ok(before) => before,
      Err(failure) if passed_over(&failure) => return Ok(None),
      Err(failure) => return Err(failure),
    };
    let inherited_before = self.query_mergeinfo(&previous_path, previous_revision, true)?;
    let after = self.query_mergeinfo(path, revision, false)?;
    let inherited_after = self.query_mergeinfo(path, revision, true)?;
    if before.is_none() && after.is_none() {
      return Ok(None);
    }
    let same = match (&inherited_before, &inherited_after) {
      (Some(first), Some(second)) => {
        Mergeinfo::equals(Some(first), Some(second), Compare::WithInheritance)
      }
      (first, _) => Mergeinfo::equals(first.as_ref(), None, Compare::WithInheritance),
    };
    if same {
      return Ok(None);
    }
    Ok(Some(Mergeinfo::diff(
      before.as_ref(),
      after.as_ref(),
      Compare::WithoutInheritance,
    )))
  }

  /// `svn_fs__get_mergeinfo_for_path`: the path's inherited mergeinfo adjusted as it inherits it, or with `raw` its
  /// nearest ancestor's as that ancestor holds it.
  fn query_mergeinfo(
    &self,
    path: &str,
    revision: Revision,
    raw: bool,
  ) -> Result<Option<Mergeinfo>, Failure> {
    let found = if raw {
      mergeinfo_as_held(
        self.index,
        self.repository,
        path,
        revision,
        MergeinfoInheritance::NearestAncestor,
      )
    } else {
      mergeinfo_at(
        self.index,
        self.repository,
        path,
        revision,
        MergeinfoInheritance::Inherited,
      )
    };
    found.map_err(|refusal| Failure::of_query(&refusal, revision))
  }

  /// `fs_mergeinfo_changed`: each path of `revision` whose `svn:mergeinfo` changed, as the mergeinfo it deleted and
  /// added against its base.
  fn catalog(&self, revision: Revision) -> Result<Catalog, Unread> {
    let changes = self
      .index
      .changes(revision)
      .map_err(|refusal| Failure::of_index(&refusal))?;
    let mut catalog = Catalog::new();
    if !changes
      .iter()
      .any(|change| change.mergeinfo_modified && change.properties.is_some())
    {
      return Ok(catalog);
    }
    let any_copy = changes
      .iter()
      .any(|change| matches!(change.action, Action::Add | Action::Replace));
    for change in changes {
      if !change.mergeinfo_modified || change.properties.is_none() {
        continue;
      }
      let base = match change.action {
        Action::Delete => continue,
        Action::Modify => Some(self.modified_base(&change.path, revision, any_copy)?),
        Action::Add | Action::Replace => change
          .copy_from
          .as_ref()
          .map(|source| (source.path.clone(), source.revision)),
      };
      if let Some(delta) = self.mergeinfo_delta(&change.path, revision, base)? {
        catalog.insert(change.path.clone(), delta);
      }
    }
    Ok(catalog)
  }

  /// A modified path's base in `fs_mergeinfo_changed`: its previous location when a copy in `revision` made it, else
  /// itself the revision before.
  fn modified_base(
    &self,
    path: &str,
    revision: Revision,
    any_copy: bool,
  ) -> Result<(String, Revision), Failure> {
    let before = (path.to_owned(), revision.saturating_sub(1));
    if !any_copy {
      return Ok(before);
    }
    let signed = i64::try_from(revision).map_err(|_| Failure::corrupt())?;
    match self.previous_location(path, signed)? {
      Some((appeared, previous_path, previous_revision)) if appeared == signed => Ok((
        previous_path,
        Revision::try_from(previous_revision).map_err(|_| Failure::corrupt())?,
      )),
      _ => Ok(before),
    }
  }

  /// One changed path's mergeinfo delta in `fs_mergeinfo_changed`: none when neither side holds a value or both hold
  /// the same bytes; a side without one takes its inherited value; both parsed, then compared without inheritance.
  fn mergeinfo_delta(
    &self,
    path: &str,
    revision: Revision,
    base: Option<(String, Revision)>,
  ) -> Result<Option<MergeinfoDelta>, Unread> {
    let before_value = match &base {
      Some((base_path, base_revision)) => self.held_mergeinfo(base_path, *base_revision)?,
      None => None,
    };
    let after_value = self.held_mergeinfo(path, revision)?;
    if before_value == after_value {
      return Ok(None);
    }
    let parse = |value: Option<&[u8]>| -> Result<Option<Mergeinfo>, Unread> {
      value
        .map(|value| Mergeinfo::parse(value).map_err(|_| Unread::Unparsed))
        .transpose()
    };
    let mut before = parse(before_value)?;
    let mut after = parse(after_value)?;
    if before_value.is_some() && after_value.is_none() {
      after = self.query_mergeinfo(path, revision, false)?;
    } else if after_value.is_some()
      && before_value.is_none()
      && let Some((base_path, base_revision)) = &base
    {
      before = self.query_mergeinfo(base_path, *base_revision, false)?;
    }
    Ok(Some(Mergeinfo::diff(
      before.as_ref(),
      after.as_ref(),
      Compare::WithoutInheritance,
    )))
  }

  /// `svn_fs_node_prop(..., SVN_PROP_MERGEINFO)`: the node's own `svn:mergeinfo`, refused when the path is absent.
  fn held_mergeinfo(&self, path: &str, revision: Revision) -> Result<Option<&[u8]>, Failure> {
    let node = self
      .node(path, revision)?
      .ok_or_else(|| Failure::not_found(path, revision))?;
    Ok(match node.properties() {
      PropertyList::Held { identity, .. } => self.repository.mergeinfo_value(&identity),
      PropertyList::Empty => None,
    })
  }
}

/// Whether `get_combined_mergeinfo_changes` passes over a refusal: a path not found or a file where a directory was
/// expected.
fn passed_over(failure: &Failure) -> bool {
  matches!(
    failure.code,
    SVN_ERR_FS_NOT_FOUND | SVN_ERR_FS_NOT_DIRECTORY
  )
}

/// `svn_mergeinfo__mergeinfo_from_segments`: each segment with a path as the source `/path` of the revisions after its
/// start through its end, a segment of revision 0 alone left out.
fn from_segments(segments: &[Segment]) -> Mergeinfo {
  let mut mergeinfo = Mergeinfo::default();
  for segment in segments {
    let Some(path) = &segment.path else {
      continue;
    };
    if segment.start == 0 && segment.end == 0 {
      continue;
    }
    let Some(range) = MergeRange::new(segment.start.saturating_sub(1), segment.end, true) else {
      continue;
    };
    let Ok(ranges) = RangeList::canonical(vec![range]) else {
      continue;
    };
    let mut one = Mergeinfo::default();
    one.insert(SourcePath::canonical(format!("/{path}").as_bytes()), ranges);
    mergeinfo = mergeinfo.merge(&one);
  }
  mergeinfo
}

/// `interesting_merge`'s test of one changed path: whether `revision` at `path` lies outside the log's own history,
/// no source of it at or above the path holding the revision.
pub(super) fn outside_history(history: &Mergeinfo, revision: Revision, path: &str) -> bool {
  !history.iter().any(|(source, ranges)| {
    std::str::from_utf8(source.as_bytes())
      .ok()
      .and_then(|source| skip_ancestor(source, path))
      .is_some()
      && ranges
        .ranges()
        .iter()
        .any(|range| revision > range.start() && revision <= range.end())
  })
}

/// One source's runs still to place, inclusive, the first first (`struct rangelist_path`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Source {
  /// The source's path.
  path: String,
  /// Its runs, each first and last revision.
  runs: std::collections::VecDeque<(Revision, Revision)>,
}

impl Source {
  /// The first run, which every source kept has.
  fn first(&self) -> (Revision, Revision) {
    self.runs.front().copied().unwrap_or_default()
  }
}

/// `combine_mergeinfo_path_lists`: a mergeinfo's ranges as runs of revisions, each with every source merged across
/// it, appended to `runs` (module documentation for the one place the C code's order is its hash table's).
pub(super) fn combine(mergeinfo: &Mergeinfo, reverse: bool, runs: &mut Vec<PathListRange>) {
  let mut sources: Vec<Source> = mergeinfo
    .iter()
    .filter_map(|(path, ranges)| {
      let runs: std::collections::VecDeque<(Revision, Revision)> = ranges
        .ranges()
        .iter()
        .map(|range| (range.start().saturating_add(1), range.end()))
        .collect();
      let path = String::from_utf8(path.as_bytes().to_vec()).ok()?;
      (!runs.is_empty()).then_some(Source { path, runs })
    })
    .collect();
  while sources.len() > 1 {
    sources.sort_by_key(Source::first);
    let (youngest, youngest_end) = sources.first().map(Source::first).unwrap_or_default();
    let sharing = sources
      .iter()
      .take_while(|source| source.first().0 == youngest)
      .count();
    let next_youngest = sources
      .get(sharing)
      .map_or(youngest, |source| source.first().0);
    let tail = if next_youngest == youngest || youngest_end < next_youngest {
      youngest_end
    } else {
      next_youngest.saturating_sub(1)
    };
    runs.push(PathListRange {
      paths: sources
        .iter()
        .take(sharing)
        .map(|source| source.path.clone())
        .collect(),
      start: youngest,
      end: tail,
      reverse,
    });
    trim(&mut sources, sharing, tail);
  }
  if let Some(last) = sources.first() {
    for &(start, end) in &last.runs {
      runs.push(PathListRange {
        paths: vec![last.path.clone()],
        start,
        end,
        reverse,
      });
    }
  }
}

/// The first `sharing` sources moved past `tail`: a first run used up is taken off, and a source left with none is
/// removed.
fn trim(sources: &mut Vec<Source>, sharing: usize, tail: Revision) {
  let mut at = 0;
  let mut left = sharing;
  while at < left {
    let Some(source) = sources.get_mut(at) else {
      break;
    };
    let Some(first) = source.runs.front_mut() else {
      break;
    };
    first.0 = tail.saturating_add(1);
    if first.0 <= first.1 {
      at = at.saturating_add(1);
      continue;
    }
    source.runs.pop_front();
    if source.runs.is_empty() {
      sources.remove(at);
      left = left.saturating_sub(1);
    } else {
      at = at.saturating_add(1);
    }
  }
}

/// `reduce_search`: the paths and range a nested walk need not search because `processed` already covers them: a path
/// covered over the whole range dropped; for a single path, a range covered at one end narrowed.
pub(super) fn reduce_search(
  paths: &mut Vec<String>,
  hist_start: &mut Revision,
  hist_end: &mut Revision,
  processed: &Mergeinfo,
) {
  let start = (*hist_start).min(*hist_end);
  let end = (*hist_start).max(*hist_end).saturating_add(1);
  let mut at = 0;
  while at < paths.len() {
    let Some(ranges) = paths
      .get(at)
      .and_then(|path| processed.get(&SourcePath::canonical(path.as_bytes())))
    else {
      at = at.saturating_add(1);
      continue;
    };
    let single = paths.len() == 1;
    let mut dropped = false;
    for range in ranges.ranges() {
      if range.start() <= start && range.end() >= end {
        paths.remove(at);
        dropped = true;
        break;
      }
      if single && narrowed(range, start, end, hist_start, hist_end) {
        break;
      }
    }
    if !dropped {
      at = at.saturating_add(1);
    }
  }
}

/// `reduce_search`'s narrowing of a single path's range by a range of `processed` covering one end of it; whether it
/// narrowed.
fn narrowed(
  range: &MergeRange,
  start: Revision,
  end: Revision,
  hist_start: &mut Revision,
  hist_end: &mut Revision,
) -> bool {
  if range.start() <= start && range.end() > start {
    let narrowed = range.end().saturating_sub(1);
    if start == *hist_start {
      *hist_start = narrowed;
    } else {
      *hist_end = narrowed;
    }
    return true;
  }
  if range.start() < end && range.end() >= end {
    if start == *hist_start {
      *hist_end = range.start();
    } else {
      *hist_start = range.start();
    }
    return true;
  }
  false
}

/// `store_search`: `processed` extended to cover the paths over the range, each range ending one past its last
/// revision.
pub(super) fn store_search(
  processed: &mut Mergeinfo,
  paths: &[String],
  hist_start: Revision,
  hist_end: Revision,
) {
  let start = hist_start.min(hist_end);
  let end = hist_start.max(hist_end).saturating_add(1);
  let Some(range) = MergeRange::new(start, end, true) else {
    return;
  };
  let mut searched = Mergeinfo::default();
  for path in paths {
    if let Ok(ranges) = RangeList::canonical(vec![range]) {
      searched.insert(SourcePath::canonical(path.as_bytes()), ranges);
    }
  }
  *processed = processed.merge(&searched);
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-613 part 2 (`svn_mergeinfo__mergeinfo_from_segments`): a segment from revision S to E at a path is the source of
  /// the revisions after S - 1 through E, so S itself lies in the log's own history; a gap, and revision 0 alone, add
  /// nothing.
  #[test]
  fn segments_are_the_revisions_from_their_starts() {
    let segment = |start, end, path: Option<&str>| Segment {
      start,
      end,
      path: path.map(str::to_owned),
    };
    let history = from_segments(&[
      segment(7, 11, Some("tags/t1")),
      segment(6, 6, None),
      segment(1, 5, Some("trunk")),
      segment(0, 0, Some("")),
    ]);
    let outside = |revision, path| outside_history(&history, revision, path);
    assert!(!outside(7, "/tags/t1/a.txt"));
    assert!(outside(6, "/tags/t1"));
    assert!(!outside(1, "/trunk/a.txt"));
    assert!(outside(0, "/trunk"));
    assert!(outside(3, "/branches/b1"));
  }

  /// A-613 part 2 (`combine_mergeinfo_path_lists`): two sources sharing a first range give one run with both, then
  /// the source with more ranges its rest; the source whose last range is used up is the one removed (where the C
  /// code removes the first of its array, which its seeded hash order decides).
  #[test]
  fn runs_combine_as_combine_mergeinfo_path_lists_means() {
    let mergeinfo = Mergeinfo::parse(b"/a:3-5,8-9\n/b:3-5").unwrap();
    let mut runs = Vec::new();
    combine(&mergeinfo, true, &mut runs);
    let run = |paths: &[&str], start, end| PathListRange {
      paths: paths.iter().map(|path| (*path).to_owned()).collect(),
      start,
      end,
      reverse: true,
    };
    assert_eq!(runs, [run(&["/a", "/b"], 3, 5), run(&["/a"], 8, 9)]);
  }
}
