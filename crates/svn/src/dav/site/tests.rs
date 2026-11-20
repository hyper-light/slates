//! A-681 part 1: the site's read side answered from the report fixtures' repository (`report::drive::green`, the history
//! stock `svnserve` was recorded against: six revisions, `trunk/secret` hidden from all but alice) to the bodies stock
//! svn 1.15.0 sent (A-609's `dav/fixtures`), each answer held to the bytes `mod_dav`'s and `mod_dav_svn`'s format
//! strings write.

use super::*;
use crate::lock::{Lock, LockEntry, LockToken};
use crate::report::drive::green::{Green, repository, rules_of};

/// Shape: the repository root's URL path the tests' URLs name.
const ROOT: &str = "/repo";
/// Shape: the UUID the tests' repository carries.
const UUID: &str = "6b4a5e2c-1d3f-4a7b-9c8e-0f1a2b3c4d5e";
/// Shape: room enough for every answer these tests ask for.
const ROOM: usize = 1 << 20;
/// Shape: bounds wide enough for every recorded body.
const BOUNDS: Bounds = Bounds {
  body_bytes: 1 << 16,
  depth: 16,
  name_bytes: 256,
  attributes: 32,
  elements: 1024,
};
/// Shape: the instant locks are judged at, after every fixture revision (2027-01-15).
const NOW: AprTime = AprTime(1_800_000_000_000_000);
/// Shape: the OPTIONS body stock svn opens a session with, asking for the activity collection.
const OPTIONS_BODY: &[u8] = include_bytes!("../fixtures/options.xml");
/// Shape: the PROPFIND `svn info` sends: six properties of a node.
const PROPFIND_INFO: &[u8] = include_bytes!("../fixtures/propfind-props.xml");
/// Shape: the PROPFIND `svn cat` sends for a file's properties.
const PROPFIND_ALL: &[u8] = include_bytes!("../fixtures/propfind-allprop.xml");
/// Shape: the PROPFIND `svn ls` sends at depth 1 where no `list` capability is listed.
const PROPFIND_KIND: &[u8] = include_bytes!("../fixtures/propfind-resourcetype.xml");
/// Shape: the PROPFIND `svn info` sends for a URL's lock.
const PROPFIND_LOCKS: &[u8] = include_bytes!("../fixtures/propfind-lockdiscovery.xml");
/// Shape: a PROPFIND of the two properties a checked-in version is read by.
const PROPFIND_VERSION: &[u8] =
  b"<?xml version=\"1.0\" encoding=\"utf-8\"?><propfind xmlns=\"DAV:\"><prop>\
<checked-in/><version-name/></prop></propfind>";

/// The fixtures' repository, with a lock table.
struct Fixture {
  /// The index.
  index: Index,
  /// The green.
  green: Green,
  /// The locks.
  locks: LockTable,
}

impl Fixture {
  /// The repository with no lock.
  fn new() -> Self {
    let (index, green) = repository();
    Self {
      index,
      green,
      locks: LockTable::new(),
    }
  }

  /// The repository as the principal `rules` compile for reads it.
  fn served<'a>(&'a self, rules: Option<&'a PrincipalRules>) -> Served<'a, Green> {
    Served {
      index: &self.index,
      repository: &self.green,
      root: ROOT,
      uuid: UUID,
      locks: &self.locks,
      rules,
      now: NOW,
    }
  }

  /// The answer to `method` on `path` below the root, as the principal `rules` compile for asks it.
  fn ask(
    &self,
    rules: Option<&PrincipalRules>,
    method: &str,
    (path, depth): (&str, Option<&str>),
    body: &[u8],
  ) -> Answer {
    let request = Request {
      method,
      path,
      depth,
      body,
    };
    answer(&self.served(rules), &request, &BOUNDS, ROOM)
  }

  /// The whole answer anonymous gets to `method` on `path`.
  fn anonymous(&self, method: &str, path: &str, depth: Option<&str>, body: &[u8]) -> HttpAnswer {
    whole(self.ask(Some(&rules_of(None)), method, (path, depth), body))
  }
}

/// The answer, which must be whole.
fn whole(answer: Answer) -> HttpAnswer {
  match answer {
    Answer::Whole(whole) => whole,
    Answer::Text { .. } => panic!("a text where a whole answer was due"),
    Answer::Report(_) => panic!("a report where a whole answer was due"),
  }
}

/// The answer's body as text.
fn text_of(answer: &HttpAnswer) -> String {
  String::from_utf8(answer.body.clone()).unwrap()
}

/// Every value of the header `name`, in order.
fn header<'h>(headers: &'h Headers, name: &str) -> Vec<&'h str> {
  headers
    .iter()
    .filter(|(found, _)| *found == name)
    .map(|(_, value)| value.as_str())
    .collect()
}

/// Every href a multistatus names, in order.
fn hrefs(body: &str) -> Vec<String> {
  body
    .split("<D:href>")
    .skip(1)
    .filter_map(|rest| rest.split("</D:href>").next())
    .filter(|href| !href.contains("!svn/ver/") && !href.contains("!svn/vcc/"))
    .map(str::to_owned)
    .collect()
}

/// A-681 (`version.c` `get_option`): do send the OPTIONS stock svn opens a session with, asking for the activity
/// collection; expect `200`, the HTTPv2 stubs built from the root, the youngest revision 6, the capabilities served
/// (`log-revprops` and, since A-726 part 2, `mergeinfo`; every other names a report or a commit this site does not
/// serve yet), mergeinfo answered `yes`, and the activity collection's href.
#[test]
fn an_options_answer_lists_the_httpv2_facts_and_no_capability_unserved() {
  let fixture = Fixture::new();
  let answer = fixture.anonymous("OPTIONS", "/trunk", None, OPTIONS_BODY);
  let headers = &answer.headers;
  assert_eq!(
    (
      answer.status,
      header(headers, "DAV"),
      header(headers, "SVN-Youngest-Rev"),
      header(headers, "SVN-Repository-Root"),
      header(headers, "SVN-Rev-Root-Stub"),
      header(headers, "SVN-Repository-MergeInfo"),
      header(headers, "SVN-Repository-UUID"),
    ),
    (
      200,
      vec![
        "1,2",
        "version-control,checkout,working-resource",
        "merge,baseline,activity,version-controlled-collection",
        "http://subversion.tigris.org/xmlns/dav/svn/log-revprops",
        "http://subversion.tigris.org/xmlns/dav/svn/mergeinfo"
      ],
      vec!["6"],
      vec!["/repo"],
      vec!["/repo/!svn/rvr"],
      vec!["yes"],
      vec![UUID],
    )
  );
  assert!(text_of(&answer).contains("<D:href>/repo/!svn/act/</D:href>"));
}

/// A-681 (`dav_get_props`, `liveprops.c`): do PROPFIND a file's revision-root resource at depth 0 with the six
/// properties `svn info` asks for; expect `207` and the bytes `mod_dav` writes: one response naming the request's URL,
/// `lp1` and `lp2` declared as first used, each value as `liveprops.c` writes it.
#[test]
fn svn_infos_propfind_of_a_file_is_answered_byte_for_byte() {
  let fixture = Fixture::new();
  let answer = fixture.anonymous(
    "PROPFIND",
    "/!svn/rvr/6/trunk/script.sh",
    Some("0"),
    PROPFIND_INFO,
  );
  let expected = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">\n\
<D:response xmlns:lp1=\"DAV:\" xmlns:lp2=\"http://subversion.tigris.org/xmlns/dav/\">\n\
<D:href>/repo/!svn/rvr/6/trunk/script.sh</D:href>\n<D:propstat>\n<D:prop>\n<lp1:resourcetype/>\n\
<lp1:getcontentlength>18</lp1:getcontentlength>\n<lp2:deadprop-count>1</lp2:deadprop-count>\n\
<lp1:version-name>1</lp1:version-name>\n<lp1:creationdate>2026-01-01T00:00:01.000000Z</lp1:creationdate>\n\
<lp1:creator-displayname>alice</lp1:creator-displayname>\n</D:prop>\n<D:status>HTTP/1.1 200 OK</D:status>\n\
</D:propstat>\n</D:response>\n</D:multistatus>\n";
  assert_eq!(
    (
      answer.status,
      header(&answer.headers, "Content-Type"),
      text_of(&answer)
    ),
    (
      207,
      vec!["text/xml; charset=\"utf-8\""],
      expected.to_owned()
    )
  );
}

/// A-681 (`dav_get_props`): do PROPFIND a directory with `svn info`'s six properties; expect its href to end in a slash,
/// the length it does not define listed under `404 Not Found` with a generated `g0` prefix declared after the live
/// properties' prefixes, and the rest found.
#[test]
fn a_property_a_directory_does_not_define_is_listed_not_found() {
  let fixture = Fixture::new();
  let answer = fixture.anonymous("PROPFIND", "/!svn/rvr/6/trunk", Some("0"), PROPFIND_INFO);
  let expected = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">\n\
<D:response xmlns:lp1=\"DAV:\" xmlns:lp2=\"http://subversion.tigris.org/xmlns/dav/\" xmlns:g0=\"DAV:\">\n\
<D:href>/repo/!svn/rvr/6/trunk/</D:href>\n<D:propstat>\n<D:prop>\n\
<lp1:resourcetype><D:collection/></lp1:resourcetype>\n<lp2:deadprop-count>1</lp2:deadprop-count>\n\
<lp1:version-name>6</lp1:version-name>\n<lp1:creationdate>2026-01-06T00:00:06.000000Z</lp1:creationdate>\n\
<lp1:creator-displayname>bob</lp1:creator-displayname>\n</D:prop>\n<D:status>HTTP/1.1 200 OK</D:status>\n\
</D:propstat>\n<D:propstat>\n<D:prop>\n<g0:getcontentlength/>\n</D:prop>\n\
<D:status>HTTP/1.1 404 Not Found</D:status>\n</D:propstat>\n</D:response>\n</D:multistatus>\n";
  assert_eq!(
    (answer.status, text_of(&answer)),
    (207, expected.to_owned())
  );
}

/// A-681 (`dav_get_allprops`, `deadprops.c`, `liveprops.c`): do PROPFIND `allprop` of a file with a property, as
/// `svn cat` does; expect its own property first in `S:`, then `resourcetype`, `mod_dav_svn`'s live properties in their
/// table's order (the digests the index holds, its safe created revision in `checked-in`), then `supportedlock` and an
/// empty `lockdiscovery`, the dead properties' prefixes declared before the live ones'.
#[test]
fn svn_cats_allprop_of_a_file_is_answered_byte_for_byte() {
  let fixture = Fixture::new();
  let text = fixture
    .index
    .node("/trunk/dir/c.bin", 1)
    .unwrap()
    .unwrap()
    .text()
    .unwrap();
  let answer = fixture.anonymous(
    "PROPFIND",
    "/!svn/rvr/1/trunk/dir/c.bin",
    Some("0"),
    PROPFIND_ALL,
  );
  let expected = format!(
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">\n\
<D:response xmlns:S=\"http://subversion.tigris.org/xmlns/svn/\" xmlns:C=\"http://subversion.tigris.org/xmlns/custom/\" \
xmlns:V=\"http://subversion.tigris.org/xmlns/dav/\" xmlns:lp1=\"DAV:\" \
xmlns:lp2=\"http://subversion.tigris.org/xmlns/dav/\">\n<D:href>/repo/!svn/rvr/1/trunk/dir/c.bin</D:href>\n\
<D:propstat>\n<D:prop>\n<S:mime-type>application/octet-stream</S:mime-type>\n<lp1:resourcetype/>\n\
<lp1:getcontentlength>256</lp1:getcontentlength>\n<lp1:getcontenttype>application/octet-stream</lp1:getcontenttype>\n\
<lp1:getetag>\"1//trunk/dir/c.bin\"</lp1:getetag>\n<lp1:creationdate>2026-01-01T00:00:01.000000Z</lp1:creationdate>\n\
<lp1:getlastmodified>Thu, 01 Jan 2026 00:00:01 GMT</lp1:getlastmodified>\n\
<lp1:checked-in><D:href>/repo/!svn/ver/1/trunk/dir/c.bin</D:href></lp1:checked-in>\n\
<lp1:version-controlled-configuration><D:href>/repo/!svn/vcc/default</D:href></lp1:version-controlled-configuration>\n\
<lp1:version-name>1</lp1:version-name>\n<lp1:creator-displayname>alice</lp1:creator-displayname>\n\
<lp2:baseline-relative-path>trunk/dir/c.bin</lp2:baseline-relative-path>\n<lp2:md5-checksum>{}</lp2:md5-checksum>\n\
<lp2:repository-uuid>{UUID}</lp2:repository-uuid>\n<lp2:deadprop-count>1</lp2:deadprop-count>\n\
<lp2:sha1-checksum>{}</lp2:sha1-checksum>\n<D:supportedlock>\n<D:lockentry>\n\
<D:lockscope><D:exclusive/></D:lockscope>\n<D:locktype><D:write/></D:locktype>\n</D:lockentry>\n</D:supportedlock>\n\
<D:lockdiscovery/>\n</D:prop>\n<D:status>HTTP/1.1 200 OK</D:status>\n</D:propstat>\n</D:response>\n\
</D:multistatus>\n",
    hex(&text.md5),
    hex(&text.sha1)
  );
  assert_eq!((answer.status, text_of(&answer)), (207, expected));
}

/// A-681 (`dav_svn__get_safe_cr`): do PROPFIND `checked-in` and `version-name` of a file a copy brought into a branch
/// and left unchanged, then of the trunk's file it came from, changed since; expect the branch's file checked in at the
/// copy's revision 3 with its created revision 1, and the trunk's at its own change, revision 6.
#[test]
fn checked_in_names_the_safe_created_revision() {
  let fixture = Fixture::new();
  let version = |path: &str| {
    let answer = fixture.anonymous("PROPFIND", path, Some("0"), PROPFIND_VERSION);
    let body = text_of(&answer);
    let checked_in = body
      .split("<lp1:checked-in><D:href>")
      .nth(1)
      .and_then(|rest| rest.split("</D:href>").next())
      .map(str::to_owned);
    let name = body
      .split("<lp1:version-name>")
      .nth(1)
      .and_then(|rest| rest.split("</lp1:version-name>").next())
      .map(str::to_owned);
    (checked_in, name)
  };
  assert_eq!(
    (
      version("/!svn/rvr/6/branches/b1/dir/b.txt"),
      version("/!svn/rvr/6/trunk/dir/b.txt")
    ),
    (
      (
        Some("/repo/!svn/ver/3/branches/b1/dir/b.txt".to_owned()),
        Some("1".to_owned())
      ),
      (
        Some("/repo/!svn/ver/6/trunk/dir/b.txt".to_owned()),
        Some("6".to_owned())
      )
    )
  );
}

/// A-681 (`mod_dav_svn`'s walker; `mod_authz_svn`): do PROPFIND a directory at depth 1 with `resourcetype`, as `svn ls`
/// does with no `list` capability listed, as anonymous and as alice; expect the directory then each child in name
/// order, collections ending in a slash, and `secret` (alice's alone) left out of anonymous's answer.
#[test]
fn a_depth_one_propfind_lists_the_children_the_principal_may_read() {
  let fixture = Fixture::new();
  let alice = rules_of(Some(b"alice"));
  let listed = |rules: &PrincipalRules| {
    let answer = whole(fixture.ask(
      Some(rules),
      "PROPFIND",
      ("/!svn/rvr/6/trunk", Some("1")),
      PROPFIND_KIND,
    ));
    (answer.status, hrefs(&text_of(&answer)))
  };
  let prefix = "/repo/!svn/rvr/6/trunk/";
  let children = ["", "a.txt", "big.txt", "dir/", "renamed.txt", "script.sh"];
  let mut alices: Vec<String> = children
    .iter()
    .map(|child| format!("{prefix}{child}"))
    .collect();
  let anonymous = alices.clone();
  alices.push(format!("{prefix}secret/"));
  assert_eq!(
    (listed(&rules_of(None)), listed(&alice)),
    ((207, anonymous), (207, alices))
  );
}

/// The text a [`Answer::Text`] streams, read from the green a piece at a time as the edge reads it, and its headers.
fn streamed(fixture: &Fixture, answer: Answer) -> (Headers, Vec<u8>) {
  let Answer::Text { headers, text } = answer else {
    panic!("a whole answer where a text was due");
  };
  let mut bytes = Vec::new();
  if let Some(text) = text {
    while let Some(piece) = fixture
      .green
      .text(&text.identity, u64::try_from(bytes.len()).unwrap())
      .filter(|piece| !piece.is_empty())
    {
      bytes.extend_from_slice(&piece);
    }
  }
  (headers, bytes)
}

/// A-681 (`repos.c` `set_headers`): do GET a file at its revision root's URL, as `svn cat` does, then at its public
/// URL, then a file with a MIME type; expect its text with a week's cache, no `Last-Modified` and its ETag for the
/// first; no cache and `Last-Modified` for the public URL; and the file's own valid type for the third.
#[test]
fn a_get_streams_a_files_text_with_set_headers_fields() {
  let fixture = Fixture::new();
  let rules = rules_of(None);
  let get = |path: &str| {
    streamed(
      &fixture,
      fixture.ask(Some(&rules), "GET", (path, None), b""),
    )
  };
  let (revision_root, text) = get("/!svn/rvr/6/trunk/script.sh");
  let (public, public_text) = get("/trunk/script.sh");
  let (typed, _) = get("/!svn/rvr/1/trunk/dir/c.bin");
  let script_headers = vec![
    ("Cache-Control", String::from("max-age=604800")),
    ("ETag", String::from("\"1//trunk/script.sh\"")),
    ("Content-Type", String::from("text/plain")),
  ];
  let public_headers = vec![
    ("Cache-Control", String::from("max-age=0")),
    (
      "Last-Modified",
      String::from("Thu, 01 Jan 2026 00:00:01 GMT"),
    ),
    ("ETag", String::from("\"1//trunk/script.sh\"")),
    ("Content-Type", String::from("text/plain")),
  ];
  assert_eq!(
    (
      revision_root,
      text.clone(),
      public,
      public_text,
      header(&typed, "Content-Type")
    ),
    (
      script_headers,
      b"#!/bin/sh\necho hi\n".to_vec(),
      public_headers,
      text,
      vec!["application/octet-stream"]
    )
  );
}

/// A-681 (`mod_authz_svn`; `repos.c` `parse_uri`, `prep_regular`; `mod_dav.c` `dav_method_propfind`): do ask for what
/// this part refuses; expect each refused with its status: a hidden file `403`, a missing one `404`, a revision past
/// the youngest `404`, a malformed special URI `500`, an unserved special resource or method `501`, an infinite or
/// unknown depth on a directory `403` and `400`, a `..` name `400`, a directory's GET `501`.
#[test]
fn every_refusal_has_its_status() {
  let fixture = Fixture::new();
  let status = |method: &str, path: &str, depth: Option<&str>| {
    fixture.anonymous(method, path, depth, PROPFIND_KIND).status
  };
  let asked = [
    ("GET", "/trunk/secret/s.txt", None),
    ("GET", "/trunk/missing.txt", None),
    ("PROPFIND", "/!svn/rvr/9/trunk", Some("0")),
    ("PROPFIND", "/!svn/rvr/x/trunk", Some("0")),
    ("PROPFIND", "/!svn/zzz", Some("0")),
    ("PROPFIND", "/!svn/me", Some("0")),
    ("REPORT", "/!svn/me", None),
    ("PROPFIND", "/trunk", None),
    ("PROPFIND", "/trunk", Some("infinity")),
    ("PROPFIND", "/trunk", Some("2")),
    ("GET", "/trunk/../trunk/a.txt", None),
    ("GET", "/trunk", None),
    ("MKCOL", "/trunk/new", None),
  ];
  let statuses: Vec<u16> = asked
    .iter()
    .map(|(method, path, depth)| status(method, path, *depth))
    .collect();
  assert_eq!(
    statuses,
    [
      403, 404, 404, 500, 500, 501, 501, 403, 403, 400, 400, 501, 501
    ]
  );
}

/// A-681 (`dav_svn__convert_err`, `dav_error_response_tag`): do PROPFIND a revision past the youngest and send a
/// REPORT; expect each refusal's `D:error` body with Subversion's error tag, its code and its words.
#[test]
fn a_refusal_that_carries_a_subversion_error_explains_itself() {
  let fixture = Fixture::new();
  let past = fixture.anonymous("PROPFIND", "/!svn/rvr/9/trunk", Some("0"), PROPFIND_KIND);
  let report = fixture.anonymous("REPORT", "/!svn/me", None, b"");
  let error = |code: u32, words: &str| {
    format!(
      "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:error xmlns:D=\"DAV:\" xmlns:m=\"http://apache.org/dav/xmlns\" \
xmlns:C=\"svn:\">\n<C:error/>\n<m:human-readable errcode=\"{code}\">\n{words}\n</m:human-readable>\n</D:error>\n"
    )
  };
  assert_eq!(
    (text_of(&past), text_of(&report)),
    (
      error(160_006, "No such revision 9"),
      error(200_007, "The requested report is unknown")
    )
  );
}

/// A-681 (`props.c` `dav_insert_coreprop`; A-627's `lockdiscovery`): do lock a file, then PROPFIND its public URL's
/// `lockdiscovery` as `svn info` does; expect its active lock in the property and the lock's owner and creation date in
/// `X-SVN-Lock-Owner` and `X-SVN-Creation-Date`, and an unlocked file's property empty with neither header.
#[test]
fn svn_infos_lockdiscovery_shows_the_lock_and_its_headers() {
  let mut fixture = Fixture::new();
  let lock = Lock {
    path: "/trunk/script.sh".to_owned(),
    token: LockToken::drawn([7; 16]),
    owner: "alice".to_owned(),
    comment: None,
    is_dav_comment: false,
    created: AprTime(1_767_225_600_000_000),
    expires: None,
  };
  let token = lock.token.as_str().to_owned();
  fixture.locks.apply(&LockEntry::Set(lock)).unwrap();
  let locked = fixture.anonymous("PROPFIND", "/trunk/script.sh", Some("0"), PROPFIND_LOCKS);
  let unlocked = fixture.anonymous("PROPFIND", "/trunk/a.txt", Some("0"), PROPFIND_LOCKS);
  let body = text_of(&locked);
  assert_eq!(
    (
      body.contains(&format!("<D:href>{token}</D:href>")),
      header(&locked.headers, "X-SVN-Lock-Owner"),
      header(&locked.headers, "X-SVN-Creation-Date"),
      text_of(&unlocked).contains("<D:lockdiscovery/>\n"),
      header(&unlocked.headers, "X-SVN-Lock-Owner"),
    ),
    (
      true,
      vec!["alice"],
      vec!["2026-01-01T00:00:00.000000Z"],
      true,
      Vec::<&str>::new()
    )
  );
}

/// A-681 (`svn_mime_type_validate`): do judge media types; expect a type and subtype taken, with parameters and a tab
/// among them, and one beginning with its slash (Subversion checks only that the slash is in the media type); and one
/// with no slash, a special character in its media type, or a control character but a tab anywhere refused.
#[test]
fn a_mime_type_is_judged_as_subversion_judges_it() {
  let judged: Vec<bool> = [
    "text/plain",
    "text/html; charset=UTF-8",
    "text/plain;\tcharset=x",
    "text",
    "/plain",
    "te(xt/plain",
    "text/pl:ain",
    "text/plain; \u{1}",
  ]
  .iter()
  .map(|value| valid_mime_type(value))
  .collect();
  assert_eq!(judged, [true, true, true, false, true, false, false, false]);
}

/// Shape: the `log-report` `svn log -v` sent (A-609's fixture): two entries at most, from revision 5 down.
const LOG_VERBOSE: &[u8] = include_bytes!("../fixtures/log-report-verbose.xml");

/// A budget of `room` bytes and as many steps, untimed: a slice as the site's callers gave one before A-749's quantum.
fn untimed(room: usize) -> Budget {
  Budget {
    room,
    steps: room,
    quantum: None,
  }
}

/// The answer a REPORT streams, a slice of `room` at a time, as the principal `rules` compile for reads it: its
/// headers, what it wrote and how it ended.
fn reported(
  fixture: &Fixture,
  rules: Option<&PrincipalRules>,
  answer: Answer,
  room: usize,
) -> (Headers, Vec<u8>, ReportProgress) {
  let Answer::Report(mut report) = answer else {
    panic!("not a report: {answer:?}");
  };
  let mut out = Vec::new();
  loop {
    match report.resume(&fixture.served(rules), &mut out, untimed(room)) {
      ReportProgress::More => {}
      ended => return (report.headers(), out, ended),
    }
  }
}

/// Each item's revision and its changed paths (each its element's name, its path and its copy source), in order.
fn log_items(body: &[u8]) -> Vec<(String, Vec<String>)> {
  let report = super::super::xml::parse(body, &BOUNDS).unwrap();
  assert!(report.is(SVN_NS, "log-report"));
  let change = |path: &super::super::xml::Element| {
    let source = path.attribute("copyfrom-path").unwrap_or_default();
    format!("{} {} {source}", path.name, path.text())
  };
  report
    .elements()
    .map(|item| {
      let revision = item
        .elements()
        .find(|child| child.name == "version-name")
        .unwrap()
        .text();
      let changed = item
        .elements()
        .filter(|child| child.name.ends_with("-path"))
        .map(change)
        .collect();
      (revision, changed)
    })
    .collect()
}

/// Shape: the namespace Subversion's own elements are in.
const SVN_NS: &str = "svn:";

/// A-681 (`dav_method_report`, `reports/log.c`): do REPORT `svn log -v`'s body on the trunk, a slice of the smallest
/// room at a time, then a log naming a missing path; expect the first `text/xml`, `log.c`'s header and end, and revisions
/// 5 and 4 with their changed paths in the revision's order, the copy's source named; and the second refused before
/// its first byte, `404`, FSFS's "File not found" in `D:error`.
#[test]
fn a_log_report_streams_its_walk_and_is_refused_before_its_first_byte() {
  let fixture = Fixture::new();
  let anonymous = rules_of(None);
  let rules = Some(&anonymous);
  let asked = fixture.ask(rules, "REPORT", ("/trunk", None), LOG_VERBOSE);
  let (headers, body, ended) = reported(&fixture, rules, asked, 1);
  let text = String::from_utf8(body.clone()).unwrap();
  let start = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<S:log-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n<S:log-item>\n";
  assert_eq!(
    (
      headers,
      ended,
      text.starts_with(start),
      text.ends_with("</S:log-item>\n</S:log-report>\n")
    ),
    (
      vec![("Content-Type", String::from("text/xml; charset=\"utf-8\""))],
      ReportProgress::Done,
      true,
      true
    )
  );
  let changed = |paths: &[&str]| {
    paths
      .iter()
      .map(|path| (*path).to_owned())
      .collect::<Vec<_>>()
  };
  assert_eq!(
    log_items(&body),
    vec![
      (
        "5".to_owned(),
        changed(&["modified-path /trunk/a.txt ", "added-path /trunk/big.txt "])
      ),
      (
        "4".to_owned(),
        changed(&[
          "modified-path /branches/b1/a.txt ",
          "deleted-path /trunk/dir/c.bin ",
          "added-path /trunk/dir/sub ",
          "added-path /trunk/dir/sub/d.txt ",
          "deleted-path /trunk/new.txt ",
          "added-path /trunk/renamed.txt /trunk/new.txt",
        ])
      ),
    ]
  );
  let missing = b"<S:log-report xmlns:S=\"svn:\"><S:path>missing.txt</S:path></S:log-report>";
  let asked = fixture.ask(rules, "REPORT", ("/trunk", None), missing);
  let (_, written, ended) = reported(&fixture, rules, asked, 1);
  let words = "File not found: revision 6, path '/trunk/missing.txt'";
  let error = format!(
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:error xmlns:D=\"DAV:\" xmlns:m=\"http://apache.org/dav/xmlns\" \
xmlns:C=\"svn:\">\n<C:error/>\n<m:human-readable errcode=\"160013\">\n{words}\n</m:human-readable>\n</D:error>\n"
  );
  let ReportProgress::Refused(refused) = ended else {
    panic!("not refused before its first byte: {ended:?}");
  };
  assert_eq!(
    (written.len(), refused.status, text_of(&refused)),
    (0, 404, error)
  );
}

/// A-681 (`dav_method_report`, `deliver_report`; the module doc's departure): do REPORT a log on a missing file, a
/// report this site does not serve yet on the trunk, and a log with a negative limit; expect `404` (no resource),
/// `501` in `mod_dav_svn`'s words, and `400`.
#[test]
fn a_report_is_refused_by_its_resource_its_name_and_its_limit() {
  let fixture = Fixture::new();
  let log: &[u8] = b"<S:log-report xmlns:S=\"svn:\"><S:path></S:path></S:log-report>";
  let dated: &[u8] = include_bytes!("../fixtures/dated-rev-report.xml");
  let negative: &[u8] =
    b"<S:log-report xmlns:S=\"svn:\"><S:limit>-1</S:limit><S:path></S:path></S:log-report>";
  let unserved = fixture.anonymous("REPORT", "/trunk", None, dated);
  let statuses = [
    fixture
      .anonymous("REPORT", "/trunk/missing.txt", None, log)
      .status,
    unserved.status,
    fixture.anonymous("REPORT", "/trunk", None, negative).status,
  ];
  assert_eq!(
    (
      statuses,
      text_of(&unserved).contains("The requested report is unknown")
    ),
    ([404, 501, 400], true)
  );
}

/// Shape: a fresh `svn checkout`'s update-report of the trunk at revision 5, as ra_serf 1.15 writes it in send-all mode
/// (`libsvn_ra_serf/update.c` `make_update_reporter`, `set_path` of the target, start-empty).
const CHECKOUT_REPORT: &[u8] = b"<S:update-report xmlns:S=\"svn:\" send-all=\"true\">\
<S:include-props>yes</S:include-props>\
<S:src-path>http://svn.example/repo/trunk</S:src-path>\
<S:target-revision>5</S:target-revision>\
<S:depth>unknown</S:depth>\
<S:entry rev=\"5\" depth=\"infinity\" start-empty=\"true\"></S:entry>\
</S:update-report>";

/// A-681 (`reports/update.c`, HTTPv2's `me` resource): do a fresh checkout's REPORT on `!svn/me`, streamed a slice of the
/// smallest room at a time and again within the whole room, and one naming a revision past the youngest; expect the
/// first `text/xml`, `update.c`'s send-all header, the target revision, the trunk's file added with its text inline
/// and the report's end, the two streams the same bytes; and the second refused before its first byte, `404`, "No such
/// revision".
#[test]
fn an_update_report_on_me_streams_a_checkout_and_refuses_a_revision_past_the_youngest() {
  let fixture = Fixture::new();
  let anonymous = rules_of(None);
  let rules = Some(&anonymous);
  let sliced = fixture.ask(rules, "REPORT", ("/!svn/me", None), CHECKOUT_REPORT);
  let (headers, body, ended) = reported(&fixture, rules, sliced, 1);
  let whole_room = fixture.ask(rules, "REPORT", ("/!svn/me", None), CHECKOUT_REPORT);
  let (_, at_once, _) = reported(&fixture, rules, whole_room, ROOM);
  let text = String::from_utf8(body.clone()).unwrap();
  assert_eq!(
    (
      headers,
      ended,
      text.contains("<S:update-report xmlns:S=\"svn:\""),
      text.contains("send-all=\"true\""),
      text.contains("<S:target-revision rev=\"5\""),
      text.contains("<S:add-file name=\"a.txt\""),
      text.contains("<S:txdelta>"),
      text.ends_with("</S:update-report>\n"),
      body == at_once,
    ),
    (
      vec![("Content-Type", String::from("text/xml; charset=\"utf-8\""))],
      ReportProgress::Done,
      true,
      true,
      true,
      true,
      true,
      true,
      true
    )
  );
  let past = String::from_utf8_lossy(CHECKOUT_REPORT).replace(
    "<S:target-revision>5</S:target-revision>",
    "<S:target-revision>99</S:target-revision>",
  );
  let refused = fixture.anonymous("REPORT", "/!svn/me", None, past.as_bytes());
  assert_eq!(
    (
      refused.status,
      String::from_utf8_lossy(&refused.body).contains("No such revision 99")
    ),
    (404, true)
  );
}

/// Shape: an `svn update` of a working copy of the trunk at revision 1 to revision 5, as ra_serf 1.15 writes it in
/// send-all mode.
fn update_report() -> Vec<u8> {
  String::from_utf8_lossy(CHECKOUT_REPORT)
    .replace(
      "rev=\"5\" depth=\"infinity\" start-empty=\"true\"",
      "rev=\"1\" depth=\"infinity\"",
    )
    .into_bytes()
}

/// An update-report's answer on `!svn/me` (`body`), each slice `budget` makes alone.
fn slices(fixture: &Fixture, body: &[u8], budget: Budget) -> Vec<Vec<u8>> {
  let anonymous = rules_of(None);
  let rules = Some(&anonymous);
  let Answer::Report(mut report) = fixture.ask(rules, "REPORT", ("/!svn/me", None), body) else {
    panic!("not a report");
  };
  let mut slices = Vec::new();
  loop {
    let mut out = Vec::new();
    let progress = report.resume(&fixture.served(rules), &mut out, budget);
    slices.push(out);
    match progress {
      ReportProgress::More => {}
      ReportProgress::Done => return slices,
      ended => panic!("ended {ended:?}"),
    }
  }
}

/// A report's whole answer, and each of its calls' bytes: a slice of the smallest room is one call, or the answer's
/// end.
fn whole_and_calls(fixture: &Fixture, body: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
  let whole = slices(fixture, body, untimed(ROOM)).concat();
  let mut calls = slices(fixture, body, untimed(1));
  calls.retain(|call| !call.is_empty());
  (whole, calls)
}

/// A-749 rule 2 on DAV (§4.17.1 "A fetch's answer": admission charges only the slice in flight): do a checkout's and an
/// update's REPORT under rooms from below one call's bytes up to many calls'; expect every slice within its room, or one
/// call alone, and every room's slices the bytes the whole room wrote (the bytes
/// `an_update_report_on_me_streams_a_checkout_and_refuses_a_revision_past_the_youngest` holds to `update.c`'s).
#[test]
fn no_slice_of_an_update_report_passes_its_room_but_by_one_call_alone() {
  let fixture = Fixture::new();
  for body in [CHECKOUT_REPORT.to_vec(), update_report()] {
    let (whole, calls) = whole_and_calls(&fixture, &body);
    assert!(calls.len() > 4, "{} calls", calls.len());
    for room in [32, 64, 128, 256, 512, 1024, 4096] {
      let sliced = slices(&fixture, &body, untimed(room));
      let past: Vec<usize> = sliced
        .iter()
        .filter(|slice| slice.len() > room && !calls.contains(slice))
        .map(Vec::len)
        .collect();
      assert_eq!(past, [0; 0], "room {room}: slices past it");
      assert!(sliced.concat() == whole, "room {room}: other bytes");
    }
  }
}

thread_local! {
  /// [`tick`]'s clock.
  static TICKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A clock one nanosecond on at every reading: a quantum of one has run by the first boundary after a slice's start.
fn tick() -> u64 {
  TICKS.with(|ticks| {
    let now = ticks.get().wrapping_add(1);
    ticks.set(now);
    now
  })
}

/// A-749 rule 3 on DAV (every slice under the step quantum): do a checkout's and an update's REPORT with room and steps
/// past the whole answer, but a quantum that has run by the first boundary between the drive's steps; expect each slice
/// one step, so one call at most, and the bytes the untimed room wrote.
#[test]
fn a_slice_of_an_update_report_ends_at_the_first_step_past_its_quantum() {
  let fixture = Fixture::new();
  let timed = Budget {
    room: ROOM,
    steps: 1,
    quantum: Some(Quantum {
      now_ns: tick,
      ns: 1,
    }),
  };
  for body in [CHECKOUT_REPORT.to_vec(), update_report()] {
    let (whole, calls) = whole_and_calls(&fixture, &body);
    let sliced = slices(&fixture, &body, timed);
    let past: Vec<usize> = sliced
      .iter()
      .filter(|slice| !slice.is_empty() && !calls.contains(slice))
      .map(Vec::len)
      .collect();
    assert_eq!(past, [0; 0], "slices of more than one call");
    assert!(sliced.concat() == whole, "other bytes");
  }
}

/// Shape: the PROPFIND of a baseline's two properties a DeltaV v1 client reads a revision's collection by.
const PROPFIND_BASELINE: &[u8] =
  b"<?xml version=\"1.0\" encoding=\"utf-8\"?><propfind xmlns=\"DAV:\"><prop>\
<baseline-collection/><version-name/></prop></propfind>";

/// DeltaV v1 (`liveprops.c` `checked-in` for `DAV_SVN_RESTYPE_VCC`): do PROPFIND the VCC, `!svn/vcc/default`, at
/// depth 0 for `checked-in` and `version-name`; expect `207`, one response naming the VCC, `checked-in` the youngest
/// baseline (`!svn/bln/6`), and `version-name`, which a VCC does not support, listed not found.
#[test]
fn deltav_v1_the_vcc_names_the_youngest_baseline() {
  let fixture = Fixture::new();
  let answer = fixture.anonymous("PROPFIND", "/!svn/vcc/default", Some("0"), PROPFIND_VERSION);
  let expected = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">\n\
<D:response xmlns:lp1=\"DAV:\" xmlns:g0=\"DAV:\">\n<D:href>/repo/!svn/vcc/default</D:href>\n<D:propstat>\n<D:prop>\n\
<lp1:checked-in><D:href>/repo/!svn/bln/6</D:href></lp1:checked-in>\n</D:prop>\n\
<D:status>HTTP/1.1 200 OK</D:status>\n</D:propstat>\n<D:propstat>\n<D:prop>\n<g0:version-name/>\n</D:prop>\n\
<D:status>HTTP/1.1 404 Not Found</D:status>\n</D:propstat>\n</D:response>\n</D:multistatus>\n";
  assert_eq!(
    (answer.status, text_of(&answer)),
    (207, expected.to_owned())
  );
}

/// DeltaV v1 (`liveprops.c` `baseline-collection`, `version-name` for a baselined version resource): do PROPFIND the
/// baseline of revision 3, `!svn/bln/3`, for its collection and name; expect the revision's collection `!svn/bc/3/`
/// and `3`.
#[test]
fn deltav_v1_a_baseline_names_its_revisions_collection() {
  let fixture = Fixture::new();
  let answer = fixture.anonymous("PROPFIND", "/!svn/bln/3", Some("0"), PROPFIND_BASELINE);
  let expected = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">\n\
<D:response xmlns:lp1=\"DAV:\">\n<D:href>/repo/!svn/bln/3</D:href>\n<D:propstat>\n<D:prop>\n\
<lp1:baseline-collection><D:href>/repo/!svn/bc/3/</D:href></lp1:baseline-collection>\n\
<lp1:version-name>3</lp1:version-name>\n</D:prop>\n<D:status>HTTP/1.1 200 OK</D:status>\n</D:propstat>\n\
</D:response>\n</D:multistatus>\n";
  assert_eq!(
    (answer.status, text_of(&answer)),
    (207, expected.to_owned())
  );
}

/// DeltaV v1 (`deadprops.c`: a baseline's dead properties are its revision's): do PROPFIND `allprop` of revision 6's
/// baseline; expect its revision properties (`svn:log` as `S:log`), `resourcetype` `D:baseline`, and no `checked-in`,
/// length or MD5, which a baseline does not define.
#[test]
fn deltav_v1_a_baselines_allprop_is_its_revisions_properties() {
  let fixture = Fixture::new();
  let answer = fixture.anonymous("PROPFIND", "/!svn/bln/6", Some("0"), PROPFIND_ALL);
  let body = text_of(&answer);
  assert_eq!(
    (
      answer.status,
      body.contains("<S:log>"),
      body.contains("<lp1:resourcetype><D:baseline/></lp1:resourcetype>"),
      body.contains("checked-in"),
      body.contains("getcontentlength"),
      body.contains("md5-checksum"),
    ),
    (207, true, true, false, false, false)
  );
}

/// DeltaV v1 (`liveprops.c`: `checked-in` is a version-controlled resource's alone): do PROPFIND a version resource,
/// `!svn/ver/1/trunk/dir/c.bin`, for `checked-in` and `version-name`, and GET it; expect `version-name` 1 found and
/// `checked-in` not found, and the GET the file's text.
#[test]
fn deltav_v1_a_version_resource_is_its_node_in_that_revision() {
  let fixture = Fixture::new();
  let path = "/!svn/ver/1/trunk/dir/c.bin";
  let answer = fixture.anonymous("PROPFIND", path, Some("0"), PROPFIND_VERSION);
  let body = text_of(&answer);
  let found = body.split("HTTP/1.1 200 OK").next().unwrap_or_default();
  let text = fixture
    .index
    .node("/trunk/dir/c.bin", 1)
    .unwrap()
    .unwrap()
    .text();
  let got = match fixture.ask(Some(&rules_of(None)), "GET", (path, None), b"") {
    Answer::Text { text, .. } => text,
    _ => panic!("no text"),
  };
  assert_eq!(
    (
      answer.status,
      found.contains("<lp1:version-name>1</lp1:version-name>"),
      found.contains("checked-in"),
      body.contains("<g0:checked-in/>"),
      got,
    ),
    (207, true, false, true, text)
  );
}

/// DeltaV v1 (`repos.c` `parse_vcc_uri`, `parse_baseline_uri`): do PROPFIND a VCC named other than `default` and the
/// baseline of a revision past the youngest; expect `mod_dav_svn`'s malformed URI (`500`) and `404`.
#[test]
fn deltav_v1_an_unknown_vcc_or_a_future_baseline_is_refused() {
  let fixture = Fixture::new();
  let other = fixture.anonymous("PROPFIND", "/!svn/vcc/other", Some("0"), PROPFIND_VERSION);
  let future = fixture.anonymous("PROPFIND", "/!svn/bln/7", Some("0"), PROPFIND_BASELINE);
  assert_eq!((other.status, future.status), (500, 404));
}
