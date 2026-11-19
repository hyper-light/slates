//! PROPPATCH, LOCK and UNLOCK's tests (A-627): the bodies and heads stock svn 1.15.0 sent for `svn propset --revprop`,
//! `svn lock -m` (with and without `--force`) and `svn unlock` (with and without `--force`), recorded on the Mac Studio
//! on 2026-11-19 (`Host`, `Connection`, `TE` and `Date` not kept); each read into the lock table's request as `mod_dav`
//! and `mod_dav_svn` read it, judged by the table's guard, and answered byte for byte as they answer; every refusal
//! before the guard with its status; and hostile bodies and headers.

use super::*;
use crate::dav::SVN_PROPS;
use crate::index::Revision;
use crate::lock::{LockEntry, LockToken, UUID_BYTES};

/// Bounds no captured body comes near.
const ROOMY: Bounds = Bounds {
  body_bytes: 1 << 16,
  depth: 16,
  name_bytes: 128,
  attributes: 16,
  elements: 1_000,
};

/// `svn propset --revprop -r 5 svn:log "a new message" URL`'s PROPPATCH.
const PROPPATCH: &[u8] = include_bytes!("../fixtures/proppatch-revprop.xml");

/// `svn lock -m "editing it" URL`'s LOCK body.
const LOCK: &[u8] = include_bytes!("../fixtures/lock.xml");

/// Its head.
const LOCK_HEAD: &str = include_str!("../fixtures/lock.head");

/// `svn lock --force -m 'x < y & "z"' URL`'s LOCK body.
const LOCK_STEAL: &[u8] = include_bytes!("../fixtures/lock-steal.xml");

/// Its head.
const LOCK_STEAL_HEAD: &str = include_str!("../fixtures/lock-steal.head");

/// `svn unlock URL`'s UNLOCK head, after a PROPFIND found the lock's token.
const UNLOCK_HEAD: &str = include_str!("../fixtures/unlock.head");

/// `svn unlock --force URL`'s UNLOCK head.
const UNLOCK_BREAK_HEAD: &str = include_str!("../fixtures/unlock-break.head");

/// The token the recording's server gave the lock it reported, which both UNLOCKs send back.
const RECORDED_TOKEN: &str = "opaquelocktoken:6b4a5e2c-0000-4000-8000-000000000001";

/// The clock at the recording, 2026-11-19T05:50:56.123456Z.
const NOW: AprTime = AprTime(1_795_067_456_123_456);

/// What the tests call the repository's filesystem in messages.
const FILESYSTEM: &str = "repo";

/// The XML of every error answer, before its own element.
const ERROR_START: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:error xmlns:D=\"DAV:\" \
  xmlns:m=\"http://apache.org/dav/xmlns\"";

/// A head of youngest revision 5 holding `/trunk` and `/trunk/f.txt`, last changed in revision 3.
struct Head;

impl HeadView for Head {
  fn youngest(&self) -> Revision {
    5
  }

  fn node(&self, path: &str) -> Option<(NodeKind, Revision)> {
    match path {
      "/" | "/trunk" => Some((NodeKind::Directory, 1)),
      "/trunk/f.txt" => Some((NodeKind::File, 3)),
      _ => None,
    }
  }
}

/// A recorded head's header `name`.
fn header<'h>(head: &'h str, name: &str) -> Option<&'h str> {
  head
    .lines()
    .skip(1)
    .filter_map(|line| line.split_once(": "))
    .find(|(found, _)| found.eq_ignore_ascii_case(name))
    .map(|(_, value)| value)
}

/// A recorded head's request path.
fn request_path(head: &str) -> &str {
  head.lines().next().unwrap().split_once(' ').unwrap().1
}

/// The LOCK a recorded head and body make for `user`, its URL's path less the repository's root.
fn recorded_lock<'a>(head: &'a str, body: &'a [u8], user: Option<&'a str>) -> LockHttp<'a> {
  LockHttp {
    path: request_path(head).strip_prefix("/repo").unwrap(),
    user,
    body,
    depth: header(head, "Depth"),
    timeout: header(head, "Timeout"),
    options: header(head, "X-SVN-Options"),
    version_name: header(head, "X-SVN-Version-Name"),
  }
}

/// The UNLOCK a recorded head makes for `user`.
fn recorded_unlock(head: &str, user: Option<&'static str>) -> UnlockHttp<'_> {
  UnlockHttp {
    path: request_path(head).strip_prefix("/repo").unwrap(),
    user,
    lock_token: header(head, "Lock-Token"),
    options: header(head, "X-SVN-Options"),
  }
}

/// A token's bytes as the owner draws them: all zero.
fn zero_draw() -> [u8; UUID_BYTES] {
  [0; UUID_BYTES]
}

/// The name of a property.
fn named(namespace: &str, name: &str) -> PropertyName {
  PropertyName {
    namespace: namespace.to_owned(),
    name: name.to_owned(),
  }
}

/// A-627 (`mod_dav.c` `dav_method_proppatch`): do decode `svn propset --revprop`'s PROPPATCH; expect `svn:log` in
/// Subversion's property namespace set to the new message.
#[test]
fn the_captured_proppatch_sets_the_log_message() {
  let expected = vec![PropChange::Set {
    name: named(SVN_PROPS, "log"),
    value: "a new message".to_owned(),
    base64: false,
  }];
  assert_eq!(proppatch(PROPPATCH, &ROOMY), Ok(expected));
}

/// A-627 (`webdav-protocol`, PROPPATCH's extension): do decode a removal, a set with `V:absent`, and a set in base64;
/// expect two removals and a set marked base64.
#[test]
fn a_removal_and_an_absent_set_are_removals_and_base64_is_marked() {
  let body = b"<D:propertyupdate xmlns:D=\"DAV:\" xmlns:V=\"http://subversion.tigris.org/xmlns/dav/\" \
    xmlns:C=\"http://subversion.tigris.org/xmlns/custom/\"><D:remove><D:prop><C:old/></D:prop></D:remove>\
    <D:set><D:prop><C:gone V:absent=\"1\"/><C:bin V:encoding=\"base64\">/w==</C:bin></D:prop></D:set>\
    </D:propertyupdate>";
  let custom = "http://subversion.tigris.org/xmlns/custom/";
  let expected = vec![
    PropChange::Remove {
      name: named(custom, "old"),
    },
    PropChange::Remove {
      name: named(custom, "gone"),
    },
    PropChange::Set {
      name: named(custom, "bin"),
      value: "/w==".to_owned(),
      base64: true,
    },
  ];
  assert_eq!(proppatch(body, &ROOMY), Ok(expected));
}

/// A-627 (`util_lock.c` `dav_lock_parse_lockinfo`, `apr_xml_to_text`): do decode both captured LOCK bodies; expect an
/// exclusive write lock whose comment is the owner's text, the markup in the forced one's comment resolved, and its
/// owner element as `mod_dav` keeps it, `DAV:` declared as namespace 0 and the text quoted again.
#[test]
fn the_captured_locks_are_exclusive_write_locks_with_their_comments() {
  let lock = |text: &str, element: &str| LockInfo {
    write: true,
    scope: Some(LockScope::Exclusive),
    owner: Some(Owner {
      text: text.to_owned(),
      element: element.to_owned(),
    }),
  };
  assert_eq!(
    lockinfo(LOCK, &ROOMY),
    Ok(lock("editing it", "<ns0:owner xmlns:ns0=\"DAV:\">editing it</ns0:owner>"))
  );
  assert_eq!(
    lockinfo(LOCK_STEAL, &ROOMY),
    Ok(lock("x < y & \"z\"", "<ns0:owner xmlns:ns0=\"DAV:\">x &lt; y &amp; \"z\"</ns0:owner>"))
  );
}

/// A-627 (`apr_xml.c` 162-169, 200-263, 795-905): do decode an owner under a root declaring a second namespace and a
/// language, the owner declaring a third whose URI holds `&`, and an empty owner; expect the echo to declare all three
/// from the last, quoted, with the inherited `xml:lang`, and the empty owner closed as an empty element.
#[test]
fn an_owner_is_echoed_with_every_declared_namespace_and_its_language() {
  let body = b"<D:lockinfo xmlns:D=\"DAV:\" xmlns:Z=\"urn:z\" xml:lang=\"en\"><D:lockscope><D:exclusive/>\
    </D:lockscope><D:locktype><D:write/></D:locktype><D:owner xmlns:Y=\"urn:y&amp;\">a&amp;b</D:owner></D:lockinfo>";
  let owner = lockinfo(body, &ROOMY).unwrap().owner.unwrap();
  let element = "<ns0:owner xml:lang=\"en\" xmlns:ns2=\"urn:y&amp;\" xmlns:ns1=\"urn:z\" xmlns:ns0=\"DAV:\">\
    a&amp;b</ns0:owner>";
  assert_eq!((owner.text.as_str(), owner.element.as_str()), ("a&b", element));
  let empty = b"<lockinfo xmlns=\"DAV:\"><owner/><lockscope><exclusive/></lockscope></lockinfo>";
  let owner = lockinfo(empty, &ROOMY).unwrap().owner.unwrap();
  assert_eq!((owner.text.as_str(), owner.element.as_str()), ("", "<ns0:owner xmlns:ns0=\"DAV:\"/>"));
}

/// A-627 (a body cut anywhere): do decode every proper prefix of the PROPPATCH and both LOCK bodies; expect each
/// refused.
#[test]
fn every_cut_of_the_captured_bodies_is_refused() {
  let patches = (1..PROPPATCH.len())
    .filter(|&cut| proppatch(&PROPPATCH[..cut], &ROOMY).is_ok())
    .count();
  let locks = [LOCK, LOCK_STEAL]
    .iter()
    .flat_map(|body| (1..body.len()).map(move |cut| &body[..cut]))
    .filter(|cut| lockinfo(cut, &ROOMY).is_ok())
    .count();
  assert_eq!((patches, locks), (0, 0));
}

/// A-627 (`util.c` `dav_get_timeout`, `dav_method_lock`'s `DAVMinTimeout` bound): do read a minute, `Infinite` before a
/// number, a word `atol` reads as 0 before five seconds, one word holding `5,Infinite`, a negative number, a word only
/// beginning `Infinite`, a word not understood before seven seconds, an expiry at second 0, and no header; expect a
/// minute past the clock's whole second, none, that second, five seconds, that second again, none, seven seconds,
/// none, and none.
#[test]
fn timeouts_read_as_mod_dav_reads_them() {
  let second = NOW.0 / MICROSECONDS_PER_SECOND;
  let at = |seconds: i64| Ok(Some(AprTime((second + seconds) * MICROSECONDS_PER_SECOND)));
  let read = [
    "Second-60",
    "Infinite, Second-4100000000",
    "Second-x, Second-5",
    "Second-5,Infinite",
    "Second--5",
    "Infinitely",
    "Seconds-5 Second-7",
    "Second--1795067456",
  ]
  .map(|header| timeout(Some(header), NOW));
  let expected = [at(60), Ok(None), at(0), at(5), at(0), Ok(None), at(7), Ok(None)];
  assert_eq!(read, expected);
  assert_eq!(timeout(None, NOW), Ok(None));
}

/// A-627 (hostile `Timeout`s): do read seconds whose expiry `apr_time_t` cannot hold, and seconds past the type's
/// range, which `atol` reads as its end; expect both refused 400.
#[test]
fn a_timeout_past_what_apr_time_t_holds_is_refused() {
  for header in ["Second-99999999999999", "Second-99999999999999999999999"] {
    let refused = timeout(Some(header), NOW).unwrap_err();
    assert_eq!(
      (refused.http_status(), refused),
      (HTTP_BAD_REQUEST, MethodRefusal::Timeout {
        value: header.to_owned()
      })
    );
  }
}

/// A-627 (`mod_dav.c` `dav_method_unlock` 3380-3399): do read bracketed tokens, one with surrounding space, an
/// unbracketed one, a lone bracket and an empty pair; expect the tokens, none, none and the empty token.
#[test]
fn lock_tokens_read_as_mod_dav_reads_them() {
  let read = ["<opaquelocktoken:abc>", " <opaquelocktoken:abc> ", "opaquelocktoken:abc", "<", "<>"].map(lock_token);
  assert_eq!(
    read,
    [Some("opaquelocktoken:abc"), Some("opaquelocktoken:abc"), None, None, Some("")]
  );
}

/// A-627 (`dav_method_lock`, `open_lockdb`, `append_locks`, `dav_lock_to_svn_lock`): do read both recorded LOCKs as
/// alice's; expect the lock table's requests: her one path with no revision, the comment, no expiry, stealing only
/// for `--force`, the depth infinity the absent header means, and the owner element kept for the answer.
#[test]
fn the_recorded_locks_are_read_into_the_lock_table_s_requests() {
  let read = |head, body| lock_request(&recorded_lock(head, body, Some("alice")), &Head, NOW, &ROOMY).unwrap();
  let plan = |comment: &str, element: &str, steal: bool| LockPlan {
    request: LockRequest {
      user: Some("alice".to_owned()),
      comment: Some(comment.as_bytes().to_vec()),
      is_dav_comment: false,
      expires: None,
      steal,
      targets: BTreeMap::from([("/trunk/f.txt".to_owned(), LockTarget::default())]),
    },
    depth_zero: false,
    owner_element: Some(element.to_owned()),
  };
  assert_eq!(
    read(LOCK_HEAD, LOCK),
    plan("editing it", "<ns0:owner xmlns:ns0=\"DAV:\">editing it</ns0:owner>", false)
  );
  assert_eq!(
    read(LOCK_STEAL_HEAD, LOCK_STEAL),
    plan("x < y & \"z\"", "<ns0:owner xmlns:ns0=\"DAV:\">x &lt; y &amp; \"z\"</ns0:owner>", true)
  );
}

/// The answer to `plan` once the lock table, holding `table`, judged it with a zero token draw.
fn judged(plan: &LockPlan, table: &LockTable, room: usize) -> HttpAnswer {
  let decision = table.lock(&plan.request, &Head, NOW, room, &mut zero_draw);
  match decision {
    Err(refusal) => lock_answer(plan, Err(&refusal), NOW, FILESYSTEM).unwrap(),
    Ok(decision) => {
      let (_, answer) = decision.answers.first().unwrap();
      lock_answer(plan, answer.as_ref(), NOW, FILESYSTEM).unwrap()
    }
  }
}

/// A-627 (`dav_method_lock` 3327-3353, `dav_lock_get_activelock`, `append_locks` 846-860): do lock the recorded path
/// as alice through the lock table; expect 200 with the creation date, the owner and the bracketed token in the
/// headers, and the body `mod_dav` writes: the activelock with depth infinity, the owner element given back, no
/// timeout, and the token, followed by the blank line its two line feeds make and no line feed after `D:prop`.
#[test]
fn a_lock_is_answered_as_mod_dav_answers_it() {
  let plan = lock_request(&recorded_lock(LOCK_HEAD, LOCK, Some("alice")), &Head, NOW, &ROOMY).unwrap();
  let answer = judged(&plan, &LockTable::new(), 1);
  let token = "opaquelocktoken:00000000-0000-4000-8000-000000000000";
  let body = [
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:prop xmlns:D=\"DAV:\">\n<D:lockdiscovery>\n<D:activelock>\n",
    "<D:locktype><D:write/></D:locktype>\n<D:lockscope><D:exclusive/></D:lockscope>\n<D:depth>infinity</D:depth>\n",
    "<ns0:owner xmlns:ns0=\"DAV:\">editing it</ns0:owner><D:timeout>Infinite</D:timeout>\n<D:locktoken>\n",
    &format!("<D:href>{token}</D:href>\n</D:locktoken>\n</D:activelock>\n\n</D:lockdiscovery>\n</D:prop>"),
  ]
  .concat();
  let expected = HttpAnswer {
    status: 200,
    headers: vec![
      ("X-SVN-Creation-Date", "2026-11-19T05:50:56.123456Z".to_owned()),
      ("X-SVN-Lock-Owner", "alice".to_owned()),
      ("Lock-Token", format!("<{token}>")),
      ("Content-Type", "text/xml; charset=\"utf-8\"".to_owned()),
    ],
    body: body.into_bytes(),
  };
  assert_eq!(answer, expected);
}

/// A-627 (`dav_get_depth`, `dav_get_timeout`, `dav_lock_get_activelock`): do lock with `Depth: 0` and a week's
/// `Timeout`; expect the lock to expire a week past the clock's whole second, and the answer to name depth 0 and the
/// week in seconds.
#[test]
fn a_lock_with_a_depth_and_a_timeout_names_both() {
  let mut http = recorded_lock(LOCK_HEAD, LOCK, Some("alice"));
  http.depth = Some("0");
  http.timeout = Some("Second-604800");
  let plan = lock_request(&http, &Head, NOW, &ROOMY).unwrap();
  let week = (NOW.0 / MICROSECONDS_PER_SECOND + 604_800) * MICROSECONDS_PER_SECOND;
  assert_eq!(plan.request.expires, Some(AprTime(week)));
  let body = String::from_utf8(judged(&plan, &LockTable::new(), 1).body).unwrap();
  assert!(body.contains("<D:depth>0</D:depth>\n"), "{body}");
  assert!(body.contains("<D:timeout>Second-604800</D:timeout>\n"), "{body}");
}

/// A LOCK of `path` as alice, `body` its body.
fn lock_of<'a>(path: &'a str, body: &'a [u8]) -> LockHttp<'a> {
  LockHttp {
    path,
    user: Some("alice"),
    body,
    depth: None,
    timeout: None,
    options: None,
    version_name: None,
  }
}

/// A `D:lockinfo` body holding `inner`.
fn lockinfo_with(inner: &str) -> Vec<u8> {
  format!("<lockinfo xmlns=\"DAV:\">{inner}</lockinfo>").into_bytes()
}

/// A-627 (`dav_method_lock`, `dav_lock_parse_lockinfo`, `append_locks`, `dav_lock_to_svn_lock`): do send each LOCK
/// `mod_dav` or `mod_dav_svn` refuses before the lock table judges it; expect each refused by kind with the status they
/// answer: 400 for the body, the depth, the version, a refresh, the root, markup in the owner, the type and the scope;
/// 409 for a missing parent; 412 for an element they do not take; 501 for no principal; 405 for nothing at the path.
#[test]
fn each_lock_refused_before_the_table_has_mod_dav_s_status() {
  let exclusive_write = "<lockscope><exclusive/></lockscope><locktype><write/></locktype>";
  let bodies = [
    lockinfo_with("<foo/>"),
    lockinfo_with("<locktype><read/></locktype>"),
    lockinfo_with("<owner>a</owner><owner>b</owner>"),
    lockinfo_with("<owner><href>mailto:a</href></owner>"),
    lockinfo_with("<lockscope><shared/></lockscope><locktype><write/></locktype>"),
    lockinfo_with("<lockscope><exclusive/></lockscope>"),
    lockinfo_with(exclusive_write),
    b"<propfind xmlns=\"DAV:\"/>".to_vec(),
  ];
  let checked = |http: LockHttp<'_>, expected: MethodRefusal, status: u16| {
    let refused = lock_request(&http, &Head, NOW, &ROOMY).unwrap_err();
    assert_eq!((refused.http_status(), refused), (status, expected));
  };
  checked(lock_of("/trunk/f.txt", &bodies[0]), MethodRefusal::UnknownElement { name: "foo".to_owned() }, 412);
  checked(lock_of("/trunk/f.txt", &bodies[1]), MethodRefusal::UnknownElement { name: "locktype".to_owned() }, 412);
  checked(lock_of("/trunk/f.txt", &bodies[2]), MethodRefusal::UnknownElement { name: "owner".to_owned() }, 412);
  checked(lock_of("/trunk/f.txt", &bodies[3]), MethodRefusal::OwnerMarkup, 400);
  checked(lock_of("/trunk/f.txt", &bodies[4]), MethodRefusal::NotExclusive, 400);
  checked(lock_of("/trunk/f.txt", &bodies[5]), MethodRefusal::NotWrite, 400);
  checked(lock_of("/trunk/missing.txt", &bodies[6]), MethodRefusal::Nonexistent { path: "/trunk/missing.txt".to_owned() }, 405);
  checked(lock_of("/nope/f.txt", &bodies[6]), MethodRefusal::ParentMissing { path: "/nope/f.txt".to_owned() }, 409);
  checked(lock_of("/trunk/f.txt/x", &bodies[6]), MethodRefusal::ParentMissing { path: "/trunk/f.txt/x".to_owned() }, 409);
  checked(lock_of("/trunk/f.txt", b""), MethodRefusal::Refresh, 400);
  let root = MethodRefusal::Body(DavRefusal::UnexpectedRoot {
    expected: "lockinfo",
    namespace: "DAV:".to_owned(),
    name: "propfind".to_owned(),
  });
  checked(lock_of("/trunk/f.txt", &bodies[7]), root, 400);
  each_header_refused_before_the_table(&bodies[6]);
}

/// The header refusals of [`each_lock_refused_before_the_table_has_mod_dav_s_status`]: a depth of 1, a version that is
/// no revision, and no principal.
fn each_header_refused_before_the_table(body: &[u8]) {
  let mut deep = lock_of("/trunk/f.txt", body);
  deep.depth = Some("1");
  let mut versioned = lock_of("/trunk/f.txt", body);
  versioned.version_name = Some("abc");
  let mut anonymous = lock_of("/trunk/f.txt", body);
  anonymous.user = None;
  let refusals = [deep, versioned, anonymous].map(|http| {
    let refused = lock_request(&http, &Head, NOW, &ROOMY).unwrap_err();
    (refused.http_status(), refused)
  });
  let expected = [
    (400, MethodRefusal::Depth { value: "1".to_owned() }),
    (400, MethodRefusal::VersionName { value: "abc".to_owned() }),
    (501, MethodRefusal::Anonymous),
  ];
  assert_eq!(refusals, expected);
}

/// A-627 (hostile LOCK bodies): do send a body with a DTD declaring an entity, and one past its bound; expect the
/// first refused 400 before any entity is read and the second 413.
#[test]
fn a_hostile_lock_body_is_refused_before_it_is_read() {
  let laughs = b"<!DOCTYPE lockinfo [<!ENTITY a \"aaaa\">]><lockinfo xmlns=\"DAV:\"><owner>&a;</owner></lockinfo>";
  let refused = lock_request(&lock_of("/trunk/f.txt", laughs), &Head, NOW, &ROOMY).unwrap_err();
  assert!(matches!(&refused, MethodRefusal::Body(DavRefusal::Xml(_))) && refused.http_status() == 400, "{refused:?}");
  let small = Bounds { body_bytes: 8, ..ROOMY };
  let refused = lock_request(&lock_of("/trunk/f.txt", LOCK), &Head, NOW, &small).unwrap_err();
  assert_eq!(refused.http_status(), 413);
}

/// The table holding bob's lock on `/trunk/f.txt` with the recorded token, made at the clock, never expiring.
fn bobs_table() -> LockTable {
  let mut table = LockTable::new();
  let lock = Lock {
    path: "/trunk/f.txt".to_owned(),
    token: LockToken::supplied(RECORDED_TOKEN).unwrap(),
    owner: "bob".to_owned(),
    comment: Some("a<b".to_owned()),
    is_dav_comment: false,
    created: NOW,
    expires: None,
  };
  table.apply(&LockEntry::Set(lock)).unwrap();
  table
}

/// An error answer's body: the Subversion error's element when `tagged`, its code and message.
fn error_body(tagged: bool, code: u32, message: &str) -> Vec<u8> {
  let element = if tagged { " xmlns:C=\"svn:\">\n<C:error/>\n" } else { ">\n" };
  format!("{ERROR_START}{element}<m:human-readable errcode=\"{code}\">\n{message}\n</m:human-readable>\n</D:error>\n")
    .into_bytes()
}

/// A-627 (`append_locks` 802-843, `dav_svn__convert_err`, `dav_error_response_tag`): do lock what bob holds, then
/// steal it; expect the first 423 with FSFS's message in an svn error, and the stolen lock answered 200.
#[test]
fn a_held_lock_is_answered_423_with_fsfs_s_message_unless_stolen() {
  let plan = lock_request(&recorded_lock(LOCK_HEAD, LOCK, Some("alice")), &Head, NOW, &ROOMY).unwrap();
  let answer = judged(&plan, &bobs_table(), 2);
  let message = "Path '/trunk/f.txt' is already locked by user 'bob' in filesystem 'repo'";
  let expected = HttpAnswer {
    status: 423,
    headers: vec![("Content-Type", "text/xml; charset=\"utf-8\"".to_owned())],
    body: error_body(true, 160_035, message),
  };
  assert_eq!(answer, expected);
  let steal = lock_request(&recorded_lock(LOCK_STEAL_HEAD, LOCK_STEAL, Some("alice")), &Head, NOW, &ROOMY).unwrap();
  assert_eq!(judged(&steal, &bobs_table(), 2).status, 200);
}

/// A-627 (`append_locks`' mapping of each guard refusal): do lock a directory, an out-of-date file, a revision past the
/// head, past the table's room, and answer a refusal for no principal; expect FSFS's message with 500 and 404 for the
/// lock errors, "Failed to create new lock." with 500 for the room, and 501 with no body for no principal.
#[test]
fn each_guard_refusal_of_a_lock_is_answered_as_append_locks_answers_it() {
  let body = lockinfo_with("<lockscope><exclusive/></lockscope><locktype><write/></locktype>");
  let answered = |path: &str, version: Option<&str>, room: usize| {
    let mut http = lock_of(path, &body);
    http.version_name = version;
    let plan = lock_request(&http, &Head, NOW, &ROOMY).unwrap();
    let answer = judged(&plan, &LockTable::new(), room);
    (answer.status, String::from_utf8(answer.body).unwrap())
  };
  let cases = [
    (answered("/trunk", None, 1), 500, error_body(true, 160_017, "'/trunk' is not a file in filesystem 'repo'")),
    (answered("/trunk/f.txt", Some("2"), 1), 500, error_body(true, 160_042, "Lock failed: newer version of '/trunk/f.txt' exists")),
    (answered("/trunk/f.txt", Some("9"), 1), 404, error_body(true, 160_006, "No such revision 9")),
    (answered("/trunk/f.txt", None, 0), 500, error_body(true, 160_000, "Failed to create new lock.")),
  ];
  for ((status, written), expected_status, expected_body) in cases {
    assert_eq!((status, written.into_bytes()), (expected_status, expected_body));
  }
  let plan = lock_request(&lock_of("/trunk/f.txt", &body), &Head, NOW, &ROOMY).unwrap();
  let anonymous = lock_answer(&plan, Err(&LockRefusal::NoUser { path: None }), NOW, FILESYSTEM).unwrap();
  assert_eq!((anonymous.status, anonymous.headers.len(), anonymous.body.len()), (501, 0, 0));
}

/// A-627 (`dav_method_unlock`, `get_locks`, `remove_lock`): do read the recorded UNLOCKs against bob's lock, as bob,
/// as alice, forced as alice and forced anonymously, then judge each by the table; expect bob's 204, alice's 423 with
/// FSFS's message, her forced one 204, and the anonymous forced one 501 with `mod_dav`'s error id.
#[test]
fn the_recorded_unlocks_are_judged_and_answered_as_mod_dav_svn_answers_them() {
  let table = bobs_table();
  let answer = |head: &'static str, user: Option<&'static str>| {
    let request = unlock_request(&recorded_unlock(head, user), &table, NOW).unwrap();
    let decision = table.unlock(&request, NOW);
    match decision {
      Err(refusal) => unlock_answer(&request, Err(&refusal), FILESYSTEM),
      Ok(decision) => unlock_answer(&request, decision.answers.first().unwrap().1.as_ref().map(|_| ()), FILESYSTEM),
    }
  };
  let done = HttpAnswer {
    status: 204,
    headers: Vec::new(),
    body: Vec::new(),
  };
  let refused = |status: u16, body: Vec<u8>| HttpAnswer {
    status,
    headers: vec![("Content-Type", "text/xml; charset=\"utf-8\"".to_owned())],
    body,
  };
  let mismatch = "User 'alice' is trying to use a lock owned by 'bob' in filesystem 'repo'";
  assert_eq!(answer(UNLOCK_HEAD, Some("bob")), done);
  assert_eq!(answer(UNLOCK_HEAD, Some("alice")), refused(423, error_body(true, 160_039, mismatch)));
  assert_eq!(answer(UNLOCK_BREAK_HEAD, Some("alice")), done);
  let anonymous = error_body(false, 405, "Anonymous lock removal is not allowed.");
  assert_eq!(answer(UNLOCK_BREAK_HEAD, None), refused(501, anonymous));
}

/// A-627 (`dav_method_unlock` 3380-3399, `dav_validate_resource_state` 1060-1073, 1255-1278, 1402-1407): do send
/// UNLOCKs with no token, an unbracketed token, a token not the lock's, for a path with no lock, for an expired lock,
/// and anonymously without breaking; expect each refused 400, and the anonymous one 403 naming the owner.
#[test]
fn each_unlock_refused_before_the_table_has_mod_dav_s_status() {
  let mut expiring = bobs_table();
  let mut lock = expiring.get_lock("/trunk/f.txt", NOW).unwrap().clone();
  lock.expires = Some(AprTime(NOW.0 - 1));
  expiring.apply(&LockEntry::Set(lock)).unwrap();
  let held = bobs_table();
  let bracketed = format!("<{RECORDED_TOKEN}>");
  let unlock = |path, user, token| UnlockHttp { path, user, lock_token: token, options: None };
  let cases = [
    (unlock("/trunk/f.txt", Some("bob"), None), &held, MethodRefusal::NoLockToken, 400),
    (unlock("/trunk/f.txt", Some("bob"), Some(RECORDED_TOKEN)), &held, MethodRefusal::MalformedLockToken { header: RECORDED_TOKEN.to_owned() }, 400),
    (unlock("/trunk/f.txt", Some("bob"), Some("<opaquelocktoken:other>")), &held, MethodRefusal::TokenMismatch { path: "/trunk/f.txt".to_owned() }, 400),
    (unlock("/trunk", Some("bob"), Some(bracketed.as_str())), &held, MethodRefusal::NoOutstandingLock { path: "/trunk".to_owned() }, 400),
    (unlock("/trunk/f.txt", Some("bob"), Some(bracketed.as_str())), &expiring, MethodRefusal::NoOutstandingLock { path: "/trunk/f.txt".to_owned() }, 400),
    (unlock("/trunk/f.txt", None, Some(bracketed.as_str())), &held, MethodRefusal::AnonymousUnlock { owner: "bob".to_owned() }, 403),
  ];
  for (http, table, expected, status) in cases {
    let refused = unlock_request(&http, table, NOW).unwrap_err();
    assert_eq!((refused.http_status(), refused), (status, expected));
  }
}

/// A-627 (`remove_lock`'s other errors): do answer an unlock the table refused for a path out of canonical form; expect
/// "Failed to remove a lock." with its code and 500, with no Subversion error element.
#[test]
fn an_unlock_s_other_refusal_is_answered_failed_to_remove_a_lock() {
  let request = UnlockRequest {
    user: Some("bob".to_owned()),
    break_lock: false,
    targets: BTreeMap::new(),
  };
  let refusal = LockRefusal::PathSyntax { path: "//x".to_owned() };
  let answer = unlock_answer(&request, Err(&refusal), FILESYSTEM);
  assert_eq!((answer.status, answer.body), (500, error_body(false, 160_005, "Failed to remove a lock.")));
}

/// A-627 (`props.c` `dav_insert_coreprop`, `svn_lock_to_dav_lock`, `get_locks`): do discover no lock, bob's lock (its
/// comment holding markup), a DAV client's comment, and a lock 100 seconds and then past its expiry; expect the empty
/// property, bob's activelock at depth 0 with his comment quoted in `D:owner` and the two headers, the DAV comment as
/// it was, and `Second-100` then `Second-0`.
#[test]
fn a_lock_is_discovered_as_get_locks_reports_it() {
  assert_eq!(
    lockdiscovery(None, NOW),
    Ok(Discovery {
      property: "<D:lockdiscovery/>\n".to_owned(),
      headers: Vec::new()
    })
  );
  let table = bobs_table();
  let mut lock = table.get_lock("/trunk/f.txt", NOW).unwrap().clone();
  let found = lockdiscovery(Some(&lock), NOW).unwrap();
  let property = [
    "<D:lockdiscovery><D:activelock>\n<D:locktype><D:write/></D:locktype>\n",
    "<D:lockscope><D:exclusive/></D:lockscope>\n<D:depth>0</D:depth>\n",
    "<D:owner xmlns:D=\"DAV:\">a&lt;b</D:owner><D:timeout>Infinite</D:timeout>\n<D:locktoken>\n",
    &format!("<D:href>{RECORDED_TOKEN}</D:href>\n</D:locktoken>\n</D:activelock>\n</D:lockdiscovery>\n"),
  ]
  .concat();
  let headers = vec![
    ("X-SVN-Creation-Date", "2026-11-19T05:50:56.123456Z".to_owned()),
    ("X-SVN-Lock-Owner", "bob".to_owned()),
  ];
  assert_eq!(found, Discovery { property, headers });
  discovered_timeouts_and_dav_comments(&mut lock);
}

/// The rest of [`a_lock_is_discovered_as_get_locks_reports_it`].
fn discovered_timeouts_and_dav_comments(lock: &mut Lock) {
  lock.comment = Some("<D:owner xmlns:D=\"DAV:\">x</D:owner>".to_owned());
  lock.is_dav_comment = true;
  lock.expires = Some(AprTime(NOW.0 + 100 * MICROSECONDS_PER_SECOND));
  let property = lockdiscovery(Some(lock), NOW).unwrap().property;
  assert!(property.contains("<D:depth>0</D:depth>\n<D:owner xmlns:D=\"DAV:\">x</D:owner><D:timeout>Second-100</D:timeout>"), "{property}");
  let later = AprTime(NOW.0 + 101 * MICROSECONDS_PER_SECOND);
  let property = lockdiscovery(Some(lock), later).unwrap().property;
  assert!(property.contains("<D:timeout>Second-0</D:timeout>"), "{property}");
}

/// A-627 (a client reads the answers): do read LOCK's answer and a PROPFIND's multistatus holding the discovery back
/// with the reader; expect both well formed, and the token the `D:href` holds, as ra_serf reads it.
#[test]
fn the_lock_answers_read_back_as_well_formed_xml() {
  let plan = lock_request(&recorded_lock(LOCK_STEAL_HEAD, LOCK_STEAL, Some("alice")), &Head, NOW, &ROOMY).unwrap();
  let answer = judged(&plan, &LockTable::new(), 1);
  let root = xml::parse(&answer.body, &ROOMY).unwrap();
  assert_eq!((root.namespace.as_str(), root.name.as_str()), ("DAV:", "prop"));
  let lock = bobs_table().get_lock("/trunk/f.txt", NOW).unwrap().clone();
  let discovery = lockdiscovery(Some(&lock), NOW).unwrap();
  let propstat = crate::dav::response::Propstat {
    status: "200 OK",
    props: vec![discovery.property],
  };
  let multistatus =
    crate::dav::response::multistatus(&[("/repo/trunk/f.txt".to_owned(), vec![propstat])], 1 << 16).unwrap();
  let read = xml::parse(&multistatus, &ROOMY).unwrap();
  let href = read
    .elements()
    .flat_map(Element::elements)
    .flat_map(Element::elements)
    .flat_map(Element::elements)
    .flat_map(Element::elements)
    .flat_map(Element::elements)
    .flat_map(Element::elements)
    .find(|element| element.is(DAV, "href"))
    .map(Element::text);
  assert_eq!(href.as_deref(), Some(RECORDED_TOKEN));
}

/// A-627 (a header httpd refuses): do answer a lock whose owner's name holds a line break; expect it refused rather
/// than written into a header.
#[test]
fn a_lock_owner_holding_a_line_break_is_never_written_into_a_header() {
  let plan = lock_request(&recorded_lock(LOCK_HEAD, LOCK, Some("alice")), &Head, NOW, &ROOMY).unwrap();
  let mut lock = bobs_table().get_lock("/trunk/f.txt", NOW).unwrap().clone();
  lock.owner = "bob\r\nX-Injected: 1".to_owned();
  assert!(matches!(
    lock_answer(&plan, Ok(&lock), NOW, FILESYSTEM),
    Err(DavRefusal::Unwritable { what: "X-SVN-Lock-Owner", .. })
  ));
  assert!(matches!(lockdiscovery(Some(&lock), NOW), Err(DavRefusal::Unwritable { .. })));
}
