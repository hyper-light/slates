//! Tests of `find_repos`'s port (A-695): what a URL leads to, and how a URL that leads nowhere is refused.

use super::find_repository;
use crate::handshake::Unserved;

/// What a URL leads to: each candidate name with its depth, longest first, and each candidate's root URL.
type Resolved = Vec<(usize, String, Vec<u8>)>;

/// `url`'s candidates, each with its root URL.
fn resolved(url: &[u8]) -> Resolved {
  let found = find_repository(url).expect("the URL leads somewhere");
  found
    .candidates()
    .map(|(depth, name)| (depth, name, found.root_url(depth)))
    .collect()
}

/// A-695. Do: resolve the URL stock `svn` 1.15.0 sent `svnserve` in the recorded `anonymous` session, one with a port
/// and a path holding an escape, one whose first segment holds an escape, a tunnel's, and one holding a `...` segment.
/// Expect: the candidate names are the path's leading runs of segments, decoded and joined by `/`, longest first, as
/// `svn_repos_find_root_path` walks up; each one's root URL is the client's URL less the components beneath it, its
/// escapes as the client wrote them; three dots climb nowhere.
#[test]
fn a_urls_candidates_are_its_leading_runs_longest_first_each_with_its_root_url() {
  let cases: [(&[u8], Resolved); 4] = [
    (
      b"svn://127.0.0.1/anon",
      vec![(1, "anon".to_owned(), b"svn://127.0.0.1/anon".to_vec())],
    ),
    (
      b"svn://127.0.0.1:3690/team/repo/a%20b",
      vec![
        (
          3,
          "team/repo/a b".to_owned(),
          b"svn://127.0.0.1:3690/team/repo/a%20b".to_vec(),
        ),
        (2, "team/repo".to_owned(), b"svn://127.0.0.1:3690/team/repo".to_vec()),
        (1, "team".to_owned(), b"svn://127.0.0.1:3690/team".to_vec()),
      ],
    ),
    (
      b"svn+ssh://h/my%20repo/x",
      vec![
        (2, "my repo/x".to_owned(), b"svn+ssh://h/my%20repo/x".to_vec()),
        (1, "my repo".to_owned(), b"svn+ssh://h/my%20repo".to_vec()),
      ],
    ),
    (
      b"svn://h/repo/...",
      vec![
        (2, "repo/...".to_owned(), b"svn://h/repo/...".to_vec()),
        (1, "repo".to_owned(), b"svn://h/repo".to_vec()),
      ],
    ),
  ];
  for (url, expected) in cases {
    assert_eq!(resolved(url), expected, "{}", String::from_utf8_lossy(url));
  }
}

/// A-695. Do: resolve URLs of another scheme, a malformed scheme part, paths climbing with `..` (once escaped), and
/// the server's root with and without separators. Expect: `svnserve`'s refusals: no `svn://` URL is `BadUrl` naming the
/// URL; a climb is `BadPath`, its escape decoded before the check; the root holds no repository, `NoRepository` naming
/// the URL.
#[test]
fn a_url_of_another_scheme_a_climb_or_the_servers_root_is_refused_as_svnserve_refuses_it() {
  let cases: [(&[u8], Unserved<'_>); 8] = [
    (
      b"http://h/repo",
      Unserved::BadUrl {
        url: b"http://h/repo",
      },
    ),
    (
      b"svnx://h/repo",
      Unserved::BadUrl {
        url: b"svnx://h/repo",
      },
    ),
    (
      b"svn:/h/repo",
      Unserved::BadUrl {
        url: b"svn:/h/repo",
      },
    ),
    (b"svn://h/repo/../other", Unserved::BadPath),
    (b"svn://h/%2E%2E/other", Unserved::BadPath),
    (b"svn://h", Unserved::NoRepository { url: b"svn://h" }),
    (b"svn://h/", Unserved::NoRepository { url: b"svn://h/" }),
    (b"svn://h//", Unserved::NoRepository { url: b"svn://h//" }),
  ];
  for (url, refused) in cases {
    assert_eq!(
      find_repository(url),
      Err(refused),
      "{}",
      String::from_utf8_lossy(url)
    );
  }
}
