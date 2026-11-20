//! Where a client's `svn://` URL leads (A-695; §4.17.4 piece 7): `svnserve`'s `find_repos` (`serve.c` 3884-3955,
//! with `skip_scheme_part` 3823 and `repos_path_valid`; Subversion 1.15.0, `serve.c` sha256 8154654e…4965) as slates
//! names its repositories, with no I/O.
//!
//! **The rule, step for step.** The URL must begin `svn://`, or `svn+NAME://` for a tunnel's scheme, else it is
//! [`Unserved::BadUrl`]. Its authority is skipped (`svnserve` without `--virtual-host`), and what follows the first `/`
//! is the path: made a canonical relative path (`svn_relpath_canonicalize`: empty and `.` names dropped), then
//! percent-decoded (`svn_path_uri_decode`). A path holding a `..` segment would climb out of the server's root, so it is
//! [`Unserved::BadPath`], checked after the decoding, so `%2E%2E` climbs too. A path with no segment names the
//! server's root, where no repository lives: [`Unserved::NoRepository`]. `svnserve` then walks up from the whole path
//! to the first directory that is a repository's root (`svn_repos_find_root_path`): here the names a repository could
//! have are the path's leading runs of segments joined by `/`, longest first ([`Found::candidates`]), since a
//! repository's name may hold a `/`. The repository's root URL, which `repos-info` sends, is the client's URL less as
//! many trailing components as the path beneath the repository holds ([`Found::root_url`],
//! `svn_path_remove_components`).
//!
//! Which candidate names a repository, and on which shard, is the caller's to find (the server's listeners).
//!
//! **Not here yet.** `svnserve` canonicalizes the client's URL first (`construct_server_baton`'s
//! `svn_uri_canonicalize_safe`), which every stock client's URL already is; a URL no stock client sends (an upper-case
//! scheme, a `.` or empty segment, a trailing `/`) is resolved here as it came (GAPS).

use crate::handshake::Unserved;
use crate::report::uri_decode_bytes;

/// Format: the scheme `skip_scheme_part` takes, `svn`, before `://` or before a tunnel's `+NAME://`.
const SCHEME: &[u8] = b"svn";
/// Format: the byte that begins a tunnel's name after the scheme (`svn+ssh`).
const TUNNEL: u8 = b'+';
/// Format: what ends the scheme part, before the authority.
const SCHEME_END: &[u8] = b"://";
/// Format: the separator of a URL's path segments.
const SEPARATOR: u8 = b'/';
/// Format: the segment that names the segment it is in (`svn_relpath_canonicalize` drops it).
const CURRENT: &[u8] = b".";
/// Format: the segment that names its parent, which would climb out of the server's root (`repos_path_valid`).
const PARENT: &[u8] = b"..";

/// Where a client's URL leads beneath the server's root, as `find_repos` resolves it before it searches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
  /// The URL as the client sent it, which a refusal names ("No repository found in" it).
  pub url: Vec<u8>,
  /// The path's segments, decoded: at least one, none of them empty, `.` or `..`.
  pub segments: Vec<Vec<u8>>,
}

impl Found {
  /// The names a repository the URL leads into could have, with how many leading segments each takes, longest first:
  /// `svn_repos_find_root_path`'s walk up from the whole path. A run that is no UTF-8 names no repository and is left
  /// out.
  pub fn candidates(&self) -> impl Iterator<Item = (usize, String)> + '_ {
    (1..=self.segments.len()).rev().filter_map(|depth| {
      let run = self.segments.get(..depth)?.join(&SEPARATOR);
      String::from_utf8(run).ok().map(|name| (depth, name))
    })
  }

  /// The root URL of the repository whose name takes the path's first `depth` segments: the client's URL less the
  /// components beneath it.
  pub fn root_url(&self, depth: usize) -> Vec<u8> {
    without_components(&self.url, self.segments.len().saturating_sub(depth))
  }
}

/// Finds where the client's `url` leads (the module doc).
///
/// # Errors
/// [`Unserved::BadUrl`] for a URL that is no `svn://` URL; [`Unserved::BadPath`] for a path that climbs with `..`;
/// [`Unserved::NoRepository`] for the server's root.
pub fn find_repository(url: &[u8]) -> Result<Found, Unserved<'_>> {
  let after_scheme = skip_scheme_part(url).ok_or(Unserved::BadUrl { url })?;
  let path = after_scheme
    .iter()
    .position(|byte| *byte == SEPARATOR)
    .and_then(|at| after_scheme.get(at..))
    .unwrap_or_default();
  let decoded = uri_decode_bytes(&canonical_relpath(path));
  let segments: Vec<&[u8]> = decoded.split(|byte| *byte == SEPARATOR).collect();
  if segments.contains(&PARENT) {
    return Err(Unserved::BadPath);
  }
  if segments.iter().all(|segment| segment.is_empty()) {
    return Err(Unserved::NoRepository { url });
  }
  Ok(Found {
    url: url.to_vec(),
    segments: segments.into_iter().map(<[u8]>::to_vec).collect(),
  })
}

/// What follows `url`'s scheme part, as `skip_scheme_part` finds it: `svn://` or `svn+NAME://`; none for any other.
fn skip_scheme_part(url: &[u8]) -> Option<&[u8]> {
  let rest = url.strip_prefix(SCHEME)?;
  let rest = if rest.first() == Some(&TUNNEL) {
    let colon = rest
      .iter()
      .position(|byte| *byte == b':')
      .unwrap_or(rest.len());
    rest.get(colon..)?
  } else {
    rest
  };
  rest.strip_prefix(SCHEME_END)
}

/// `path` as `svn_relpath_canonicalize` makes it: its empty and `.` segments dropped, the rest joined by single
/// separators.
fn canonical_relpath(path: &[u8]) -> Vec<u8> {
  let segments: Vec<&[u8]> = path
    .split(|byte| *byte == SEPARATOR)
    .filter(|segment| !segment.is_empty() && *segment != CURRENT)
    .collect();
  segments.join(&SEPARATOR)
}

/// `url` with its last `count` components removed, as `svn_path_remove_components` removes them: each cut at the last
/// separator, a leading one kept.
fn without_components(url: &[u8], count: usize) -> Vec<u8> {
  let mut kept = url;
  for _ in 0..count {
    kept = match kept.iter().rposition(|byte| *byte == SEPARATOR) {
      Some(0) => kept.get(..1).unwrap_or_default(),
      Some(at) => kept.get(..at).unwrap_or_default(),
      None => kept.get(..0).unwrap_or_default(),
    };
  }
  kept.to_vec()
}

#[cfg(test)]
mod tests;
