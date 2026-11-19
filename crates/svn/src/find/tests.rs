//! Tests of `find_repos`'s port (A-695): what a URL leads to, and how a URL that leads nowhere is refused.

use super::{Found, find_repository};
use crate::handshake::Unserved;

/// A-695. Do: resolve the URLs stock `svn` 1.15.0 sent `svnserve` in the recorded sessions, one with a port and a path
/// beneath the repository, one whose name and path hold escapes, a tunnel's, and one holding a `...` segment. Expect:
/// each names the repository by its path's first segment, decoded, and its root URL is the client's URL less the
/// components beneath the repository, its escapes as the client wrote them; three dots climb nowhere.
#[test]
fn a_url_names_its_repository_by_its_first_segment_and_its_root_by_what_is_beneath() {
  let cases: [(&[u8], &str, &[u8]); 6] = [
    (b"svn://127.0.0.1/anon", "anon", b"svn://127.0.0.1/anon"),
    (b"svn://127.0.0.1/missing", "missing", b"svn://127.0.0.1/missing"),
    (
      b"svn://127.0.0.1:3690/repo/trunk/a%20b",
      "repo",
      b"svn://127.0.0.1:3690/repo",
    ),
    (b"svn://h/my%20repo/x", "my repo", b"svn://h/my%20repo"),
    (b"svn+ssh://h/repo/trunk", "repo", b"svn+ssh://h/repo"),
    (b"svn://h/repo/.../x", "repo", b"svn://h/repo"),
  ];
  for (url, name, root_url) in cases {
    assert_eq!(
      find_repository(url),
      Ok(Found {
        url: url.to_vec(),
        name: name.to_owned(),
        root_url: root_url.to_vec(),
      }),
      "{}",
      String::from_utf8_lossy(url)
    );
  }
}

/// A-695. Do: resolve URLs of another scheme, a malformed scheme part, paths climbing with `..` (once escaped), and
/// the server's root with and without separators. Expect: `svnserve`'s refusals: no `svn://` URL is `BadUrl` naming the
/// URL; a climb is `BadPath`, its escape decoded before the check; the root holds no repository, `NoRepository` naming
/// the URL.
#[test]
fn a_url_of_another_scheme_a_climb_or_the_servers_root_is_refused_as_svnserve_refuses_it() {
  let cases: [(&[u8], Unserved<'_>); 8] = [
    (b"http://h/repo", Unserved::BadUrl { url: b"http://h/repo" }),
    (b"svnx://h/repo", Unserved::BadUrl { url: b"svnx://h/repo" }),
    (b"svn:/h/repo", Unserved::BadUrl { url: b"svn:/h/repo" }),
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
