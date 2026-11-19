//! Paths as a dump stream and the index name them (A-628). A dump's `Node-path` and `Node-copyfrom-path` are paths
//! relative to the repository's root, the root the empty path (`svn_relpath`); the index names a node by its absolute
//! path, the root `/` (`svn_fspath`). An import reads a dump's path as `svnadmin load` reads it, through
//! `svn_relpath_canonicalize` (`load-fs-vtable.c` `make_node_baton`: empty and `.` names dropped, so a leading, a
//! trailing or a doubled `/` goes; `dirent_uri.c` `canonicalize`, 297-576 at `d434dc14`), and an export writes the
//! relative form `dump.c` writes. The order an export visits a revision's changed paths in is
//! `svn_path_compare_paths`'s (`path.c` 418-454), the order `svn_repos_replay2`'s path driver sorts them in.

use std::cmp::Ordering;

/// The byte between a path's names.
/// Format: Subversion's separator (`svn_relpath_join`, `svn_fspath__join`).
pub(super) const SEPARATOR: u8 = b'/';

/// The root's absolute path.
/// Format: `svn_fspath__is_canonical`'s root, `/`.
pub(super) const ROOT: &str = "/";

/// The name `svn_relpath_canonicalize` drops besides the empty one: the directory itself.
/// Format: `dirent_uri.c` `canonicalize` skips a segment that is empty or `.` (its "noop segment").
const CURRENT_DIRECTORY: &[u8] = b".";

/// How `svn_path_compare_paths` orders two paths (`path.c` 418-454): byte by byte, but where they first differ a `/`
/// sorts before any other byte, so a directory's children come right after it.
pub(super) fn compare_paths(first: &[u8], second: &[u8]) -> Ordering {
  let common = first
    .iter()
    .zip(second)
    .take_while(|(left, right)| left == right)
    .count();
  match (first.get(common).copied(), second.get(common).copied()) {
    (None, None) => Ordering::Equal,
    (Some(SEPARATOR), None) | (Some(_), Some(SEPARATOR)) => Ordering::Greater,
    (None, Some(SEPARATOR)) | (Some(SEPARATOR), Some(_)) => Ordering::Less,
    (left, right) => left.unwrap_or(0).cmp(&right.unwrap_or(0)),
  }
}

/// A dump's path as `svn_relpath_canonicalize` leaves it: its names joined by `/`, empty and `.` names dropped.
pub(super) fn canonical_relpath(path: &[u8]) -> Vec<u8> {
  let mut canonical = Vec::with_capacity(path.len());
  let names = path
    .split(|&byte| byte == SEPARATOR)
    .filter(|name| !name.is_empty() && *name != CURRENT_DIRECTORY);
  for name in names {
    if !canonical.is_empty() {
      canonical.push(SEPARATOR);
    }
    canonical.extend_from_slice(name);
  }
  canonical
}

/// The absolute path of a relative one: `/` for the root's empty path, `/` and the path otherwise.
pub(super) fn absolute(relpath: &str) -> String {
  let mut path = String::with_capacity(relpath.len().saturating_add(1));
  path.push_str(ROOT);
  path.push_str(relpath);
  path
}

/// The relative path of an absolute one: empty for the root.
pub(super) fn relative(fspath: &str) -> &str {
  fspath.strip_prefix(ROOT).unwrap_or(fspath)
}

/// The directory a relative path stands in: empty for a name in the root.
pub(super) fn dirname(relpath: &str) -> &str {
  relpath
    .rfind(char::from(SEPARATOR))
    .and_then(|end| relpath.get(..end))
    .unwrap_or("")
}

/// The last name of a relative path.
pub(super) fn basename(relpath: &str) -> &str {
  relpath
    .rfind(char::from(SEPARATOR))
    .and_then(|end| relpath.get(end.saturating_add(1)..))
    .unwrap_or(relpath)
}

/// `name` joined beneath the relative path `directory` (`svn_relpath_join`).
pub(super) fn join(directory: &str, name: &str) -> String {
  if directory.is_empty() {
    return name.to_owned();
  }
  let mut path = String::with_capacity(directory.len().saturating_add(name.len()).saturating_add(1));
  path.push_str(directory);
  path.push(char::from(SEPARATOR));
  path.push_str(name);
  path
}

/// The longest relative path that is `first` or a directory of it and also `second` or a directory of it
/// (`svn_relpath_get_longest_ancestor`): empty when they share no name.
pub(super) fn longest_ancestor<'a>(first: &'a str, second: &str) -> &'a str {
  let mut longest = 0;
  for (end, _) in first.match_indices(char::from(SEPARATOR)).chain([(first.len(), "")]) {
    let Some(ancestor) = first.get(..end) else {
      break;
    };
    if !is_at_or_beneath(second, ancestor) {
      break;
    }
    longest = end;
  }
  first.get(..longest).unwrap_or("")
}

/// Whether the relative path `path` is `ancestor` or stands beneath it; everything stands beneath the root's empty
/// path.
pub(super) fn is_at_or_beneath(path: &str, ancestor: &str) -> bool {
  if ancestor.is_empty() {
    return true;
  }
  path.strip_prefix(ancestor).is_some_and(|rest| {
    rest.is_empty() || rest.as_bytes().first() == Some(&SEPARATOR)
  })
}

/// The number of names in a relative path that is not empty (`path_driver.c` `count_components`).
pub(super) fn names(relpath: &str) -> usize {
  relpath.split(char::from(SEPARATOR)).count()
}

/// The directories above the absolute path `fspath` other than the root, deepest first, each with the rest of the
/// path beneath it, its `/` first: `/a/b/c` gives `/a/b` with `/c`, then `/a` with `/b/c`.
pub(super) fn ancestors_deepest_first(fspath: &str) -> impl Iterator<Item = (&str, &str)> {
  fspath
    .rmatch_indices(char::from(SEPARATOR))
    .filter(|(end, _)| *end > 0)
    .filter_map(move |(end, _)| Some((fspath.get(..end)?, fspath.get(end..)?)))
}

/// The absolute path standing at `rest` (beginning with `/`) beneath the absolute path `directory`.
pub(super) fn beneath(directory: &str, rest: &str) -> String {
  if directory == ROOT {
    return rest.to_owned();
  }
  let mut path = String::with_capacity(directory.len().saturating_add(rest.len()));
  path.push_str(directory);
  path.push_str(rest);
  path
}
