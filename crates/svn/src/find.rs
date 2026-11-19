//! Where a client's `svn://` URL leads (A-695; §4.17.4 piece 7): `svnserve`'s `find_repos` (`serve.c` 3884-3955,
//! with `skip_scheme_part` 3823 and `repos_path_valid`; Subversion 1.15.0, `serve.c` sha256 8154654e…4965) as slates
//! names its repositories, with no I/O.
//!
//! **The rule, step for step.** The URL must begin `svn://`, or `svn+NAME://` for a tunnel's scheme, else it is
//! [`Unserved::BadUrl`]. Its authority is skipped (`svnserve` without `--virtual-host`), and what follows the first `/`
//! is the path: made a canonical relative path (`svn_relpath_canonicalize`: empty and `.` names dropped), then
//! percent-decoded (`svn_path_uri_decode`). A path holding a `..` segment would climb out of the server's root, so it is
//! [`Unserved::BadPath`], checked after the decoding, so `%2E%2E` climbs too. `svnserve` then searches the path's
//! prefixes for a repository's root (`svn_repos_find_root_path`); here the first segment names the repository, as the
//! HTTP edge's Subversion site takes it (`SVNParentPath`'s rule), since a name routes to the partition its hash names and
//! a longest-prefix search would ask every partition (GAPS). A path with no segment names the server's root, where no
//! repository lives: [`Unserved::NoRepository`]. The repository's root URL, which `repos-info` sends, is the client's
//! URL less as many trailing components as the path beneath the repository holds (`svn_path_remove_components`).
//!
//! Whether a repository of that name exists, and on which shard, is the caller's to find (the server's listener).
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

/// Where a client's URL leads beneath the server's root, as `find_repos` resolves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
  /// The URL as the client sent it, which a refusal names: on the shard that owns the name, a repository of none is
  /// "No repository found in" it.
  pub url: Vec<u8>,
  /// The repository's name: the first segment of the URL's path, decoded.
  pub name: String,
  /// The repository's root URL, `repos-info`'s: the client's URL less as many trailing components as the path beneath
  /// the repository holds.
  pub root_url: Vec<u8>,
}

/// Finds where the client's `url` leads (the module doc).
///
/// # Errors
/// [`Unserved::BadUrl`] for a URL that is no `svn://` URL; [`Unserved::BadPath`] for a path that climbs with `..`;
/// [`Unserved::NoRepository`] for the server's root, and for a first segment no repository's name can be (not UTF-8).
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
  let no_repository = Unserved::NoRepository { url };
  let first = segments
    .first()
    .filter(|segment| !segment.is_empty())
    .ok_or(no_repository)?;
  let name = std::str::from_utf8(first).map_err(|_| no_repository)?;
  let beneath = segments.len().saturating_sub(1);
  Ok(Found {
    url: url.to_vec(),
    name: name.to_owned(),
    root_url: without_components(url, beneath),
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
