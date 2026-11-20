//! HTTPv2 commits (A-681 part C): the requests ra_serf 1.15 sends for a commit, driven through the commit drive over an
//! empty repository, the work landed by the commit step.

use std::collections::BTreeMap;

use super::*;
use crate::commit::revision::Revision as RevisionToCommit;
use crate::dump::Uuid;
use crate::lock::{AprTime, CommitLocks};
use crate::migration::{Repository, Room};

/// Shape: room enough for every body these tests send.
const ROOM: usize = 1 << 20;
/// Shape: the XML reader's bounds for a PROPPATCH body.
const BOUNDS: Bounds = Bounds {
  body_bytes: ROOM,
  depth: 16,
  name_bytes: 256,
  attributes: 1 << 12,
  elements: 1 << 12,
};
/// Shape: the test repository's UUID.
const UUID: &[u8] = b"00000000-0000-4000-8000-000000000681";
/// Shape: the instant the repository was made at.
const CREATED: &[u8] = b"2026-11-20T00:00:00.000000Z";
/// Shape: the owner's instant the commit is judged and stamped at.
const NOW: AprTime = AprTime(1_795_100_000_000_000);
/// Shape: the POST body ra_serf 1.15 sends for `svn commit -m first` (`create_txn_post_body`).
const POST: &[u8] = b"(create-txn-with-props (7 svn:log 5 first 10 svn:author 3 eve))";
/// Shape: an svndiff (version 0) making `hello\n` from nothing: one window, one new-data instruction.
const HELLO_DELTA: &[u8] = b"SVN\x00\x00\x00\x06\x01\x06\x86hello\n";
/// Shape: a PROPPATCH setting `svn:eol-style` and a custom property in base64, as ra_serf writes it.
const PROPPATCH: &[u8] = b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<D:propertyupdate xmlns:D=\"DAV:\" xmlns:V=\"http://subversion.tigris.org/xmlns/dav/\" \
xmlns:C=\"http://subversion.tigris.org/xmlns/custom/\" xmlns:S=\"http://subversion.tigris.org/xmlns/svn/\">\
<D:set><D:prop><S:eol-style>native</S:eol-style>\
<C:colour V:encoding=\"base64\">Ymx1ZQ==</C:colour></D:prop></D:set></D:propertyupdate>";

/// An empty repository: revision 0, its root alone.
fn empty() -> Repository {
  let room = Room {
    entries: ROOM,
    bytes: 1 << 30,
  };
  Repository::new(Uuid::parse(UUID).unwrap(), CREATED, room).unwrap()
}

/// A transaction's setup over `repository`, alice committing, its revision properties from [`POST`].
fn setup(repository: &Repository) -> Setup {
  let _ = repository;
  Setup {
    base_path: String::from("/"),
    repository_url: String::from("http://svn.example/repo"),
    filesystem: String::from("repo"),
    transaction: String::from("0-1"),
    locks: CommitLocks {
      user: Some(String::from("alice")),
      presented: BTreeMap::new(),
      keep: false,
    },
    now: NOW,
    room: ROOM,
    rules: None,
    revision_properties: transaction_properties(POST, Some("alice")).unwrap(),
  }
}

/// Every hook due passed.
fn passed(_: &Due) -> Result<(), Vec<(u32, String)>> {
  Ok(())
}

/// A-681 (`create_txn.c`, `skel.c`): do read the POST's skel in its explicit-length form, its implicit form and the
/// bare `( create-txn )`, and a malformed one; expect `svn:log` kept, the client's `svn:author` replaced by the
/// principal's, nothing from the bare form but the author, and the malformed one refused.
#[test]
fn a_posts_skel_names_the_revision_properties_the_principal_authors() {
  let alice = |body: &[u8]| transaction_properties(body, Some("alice"));
  let expected = vec![
    (b"svn:author".to_vec(), b"alice".to_vec()),
    (b"svn:log".to_vec(), b"first".to_vec()),
  ];
  assert_eq!(
    (
      alice(POST),
      alice(b"( create-txn-with-props ( svn:log first ) )"),
      alice(b"( create-txn )"),
      alice(b"( create-txn-with-props ( svn:log ) )").is_err(),
      alice(b"(create-txn").is_err(),
    ),
    (
      Ok(expected.clone()),
      Ok(expected),
      Ok(vec![(b"svn:author".to_vec(), b"alice".to_vec())]),
      true,
      true,
    )
  );
}

/// A-681 (`commit.c` as ra_serf drives it; `repos.c` and `mod_dav.c`'s methods): do begin a transaction over an empty
/// repository, MKCOL `/trunk`, PUT `/trunk/a.txt`'s text as an added file, PROPPATCH two of its properties, MERGE, and
/// land the accepted work with the commit step. Expect `201`, `201`, `207` naming both properties, the work accepted,
/// and revision 1 committed holding the directory and the file, the file's text `hello\n`, its log alice's message.
#[test]
fn a_commit_over_http_adds_a_directory_and_a_file_and_lands_as_revision_one() {
  let mut repository = empty();
  let hooks: Hooks<'_> = &passed;
  let world = World {
    index: repository.index(),
    green: &repository,
    locks: repository.locks(),
  };
  let mut commit = DavCommit::begin(setup(&repository), &world, hooks).unwrap();
  let made = commit.mkcol(&world, hooks, "/trunk").status;
  let put = Put {
    body: HELLO_DELTA,
    base_md5: None,
    result_md5: Some("b1946ac92492d2347c6235b4d2611184"),
    version_name: None,
  };
  let written = commit.put(&world, hooks, "/trunk/a.txt", &put).status;
  let patched = commit.proppatch(&world, hooks, ("/trunk/a.txt", None), (PROPPATCH, &BOUNDS));
  let patched_text = String::from_utf8_lossy(&patched.body).into_owned();
  let Merged::Accepted(work) = commit.merge(&world, hooks) else {
    panic!("the MERGE was refused");
  };
  let paths: Vec<String> = work
    .record
    .changes
    .iter()
    .map(|row| row.path.clone())
    .collect();
  let texts: Vec<Vec<u8>> = work.texts.values().cloned().collect();
  let log = work
    .revision_properties
    .iter()
    .find(|(name, _)| name.as_slice() == b"svn:log")
    .map(|(_, value)| value.clone());
  let committed = repository.commit(RevisionToCommit::of_work(*work, NOW).unwrap());
  assert_eq!(
    (
      made,
      written,
      patched.status,
      patched_text.contains("<ns1:eol-style/>") && patched_text.contains("<ns2:colour/>"),
      paths,
      texts,
      log,
      committed,
      commit.over(),
    ),
    (
      201,
      201,
      207,
      true,
      vec![String::from("/trunk"), String::from("/trunk/a.txt")],
      vec![b"hello\n".to_vec()],
      Some(b"first".to_vec()),
      Ok(1),
      true,
    )
  );
}

/// A-681 (`dav_svn__convert_err`; the early-error rule): do PUT a file as opened at revision 0 where none exists, then
/// MKCOL in the same transaction; expect the first refused `404` with FSFS's "File not found" (160013) in `D:error`,
/// and the second `404` "No such transaction", the transaction over.
#[test]
fn a_refused_request_ends_its_transaction() {
  let repository = empty();
  let hooks: Hooks<'_> = &passed;
  let world = World {
    index: repository.index(),
    green: &repository,
    locks: repository.locks(),
  };
  let mut commit = DavCommit::begin(setup(&repository), &world, hooks).unwrap();
  let put = Put {
    body: HELLO_DELTA,
    base_md5: None,
    result_md5: None,
    version_name: Some(0),
  };
  let refused = commit.put(&world, hooks, "/missing.txt", &put);
  let after = commit.mkcol(&world, hooks, "/trunk");
  let words = |answer: &HttpAnswer| String::from_utf8_lossy(&answer.body).into_owned();
  assert_eq!(
    (
      refused.status,
      words(&refused).contains("errcode=\"160013\""),
      after.status,
      words(&after).contains("No such transaction '0-1'"),
      commit.over(),
    ),
    (404, true, 404, true, true)
  );
}

/// A-681 (RFC 4918 §10.4; `libsvn_ra_serf/commit.c` `setup_put_headers`): do read the `If` header ra_serf sends with a
/// locked file's PUT, a tagged list naming a resource and two tokens, and one naming no lock token. Expect the tokens
/// alone, in order, and none from the last.
#[test]
fn an_if_header_presents_its_lock_tokens_and_no_resource_tag() {
  assert_eq!(
    (
      DavCommit::if_tokens("(<opaquelocktoken:6b4a5e2c-0000>)"),
      DavCommit::if_tokens(
        "<http://svn.example/repo/a> (<opaquelocktoken:one>) (Not <opaquelocktoken:two>)"
      ),
      DavCommit::if_tokens("([\"etag\"])"),
    ),
    (
      vec!["opaquelocktoken:6b4a5e2c-0000"],
      vec!["opaquelocktoken:one", "opaquelocktoken:two"],
      Vec::<&str>::new(),
    )
  );
}
