use super::{
  Coding, Field, Framing, Head, HeadReader, HeadRefused, Malformed, Progress, Target, Version,
  media_type,
};
use crate::reply::Status;
use crate::{Admission, MAX_LINE, to_u64};

/// Room enough for every head these tests read whole.
const ROOM: u64 = 1 << 20;

/// Reads `input` in one call with [`ROOM`].
fn read(input: &[u8]) -> Result<Progress<Head<'_>>, HeadRefused> {
  HeadReader::new().read(input, &mut Admission::new(ROOM))
}

/// The head `input` holds whole, and the bytes it took.
fn whole(input: &[u8]) -> (Head<'_>, usize) {
  match read(input) {
    Ok(Progress::Complete { value, consumed }) => (value, consumed),
    other => panic!("{:?} read {other:?}", String::from_utf8_lossy(input)),
  }
}

/// The refusal `input` meets.
fn refused(input: &[u8]) -> HeadRefused {
  match read(input) {
    Err(refused) => refused,
    other => panic!("{:?} read {other:?}", String::from_utf8_lossy(input)),
  }
}

/// A request with `fields` (each line without its CRLF) after `line`, ended by its empty line.
fn request(line: &str, fields: &[&str]) -> Vec<u8> {
  let mut out = format!("{line}\r\n");
  for field in fields {
    out.push_str(field);
    out.push_str("\r\n");
  }
  out.push_str("\r\n");
  out.into_bytes()
}

/// A-595: do read the head stock git sends for an advertisement. Expect its method, target, version and authority, its
/// fields in order with their names as sent and their values trimmed, one field found by any case, no body, the
/// connection kept, and every byte taken.
#[test]
fn a_get_reads_its_line_its_fields_and_its_defaults() {
  let input = request(
    "GET /team/repo.git/info/refs?service=git-upload-pack HTTP/1.1",
    &[
      "Host: example.com:8443",
      "User-Agent: git/2.50.1",
      "Git-Protocol:  version=2 \t",
    ],
  );
  let (head, consumed) = whole(&input);
  let expected = Head {
    method: b"GET",
    target: Target::Path {
      path: b"/team/repo.git/info/refs",
      query: Some(b"service=git-upload-pack"),
    },
    version: Version::Http11,
    authority: Some(b"example.com:8443"),
    fields: vec![
      (b"Host", b"example.com:8443"),
      (b"User-Agent", b"git/2.50.1"),
      (b"Git-Protocol", b"version=2"),
    ],
    framing: Framing::None,
    coding: Coding::Identity,
    keep_alive: true,
    expect_continue: false,
  };
  assert_eq!((&head, consumed), (&expected, input.len()));
  assert_one_field_found_in_any_case(&head);
}

/// The advertisement's head's one-value lookups: `Git-Protocol` by either case, `Content-Type` absent.
fn assert_one_field_found_in_any_case(head: &Head<'_>) {
  assert_eq!(head.field(b"git-protocol"), Ok(Some(&b"version=2"[..])));
  assert_eq!(head.field(b"GIT-PROTOCOL"), Ok(Some(&b"version=2"[..])));
  assert_eq!(head.field(b"content-type"), Ok(None));
}

/// A-595: do read POSTs framed each way and coded each way. Expect `Content-Length`'s value, chunked, gzip under either
/// of its names and `identity` read as no coding.
#[test]
fn a_post_reads_its_framing_and_its_coding() {
  let line = "POST /r.git/git-upload-pack HTTP/1.1";
  let cases: [(&[&str], Framing, Coding); 5] = [
    (
      &["Host: h", "Content-Length: 1234"],
      Framing::Length(1234),
      Coding::Identity,
    ),
    (
      &["Host: h", "Transfer-Encoding: chunked"],
      Framing::Chunked,
      Coding::Identity,
    ),
    (
      &["Host: h", "Content-Length: 9", "Content-Encoding: gzip"],
      Framing::Length(9),
      Coding::Gzip,
    ),
    (
      &[
        "Host: h",
        "Transfer-Encoding: CHUNKED",
        "Content-Encoding: x-gzip",
      ],
      Framing::Chunked,
      Coding::Gzip,
    ),
    (
      &["Host: h", "Content-Length: 0", "Content-Encoding: identity"],
      Framing::Length(0),
      Coding::Identity,
    ),
  ];
  for (fields, framing, coding) in cases {
    let input = request(line, fields);
    let (head, _) = whole(&input);
    assert_eq!((head.framing, head.coding), (framing, coding), "{fields:?}");
  }
}

/// A-595: do read HTTP/1.0 and HTTP/1.1 requests with and without `Connection`. Expect HTTP/1.0 to need no `Host` and to
/// close unless it asks `keep-alive`, and HTTP/1.1 to stay open unless it asks `close`, which wins over `keep-alive`.
#[test]
fn the_connection_is_kept_as_each_version_and_its_connection_field_say() {
  let cases: [(&str, &[&str], bool); 6] = [
    ("GET / HTTP/1.0", &[], false),
    ("GET / HTTP/1.0", &["Connection: keep-alive"], true),
    ("GET / HTTP/1.0", &["Connection: Keep-Alive, close"], false),
    ("GET / HTTP/1.1", &["Host: h"], true),
    ("GET / HTTP/1.1", &["Host: h", "Connection: close"], false),
    (
      "GET / HTTP/1.1",
      &["Host: h", "connection: keep-alive", "Connection: CLOSE"],
      false,
    ),
  ];
  for (line, fields, kept) in cases {
    let input = request(line, fields);
    let (head, _) = whole(&input);
    assert_eq!(head.keep_alive, kept, "{line} {fields:?}");
  }
  let (head, _) = whole(b"GET / HTTP/1.0\r\n\r\n");
  assert_eq!((head.version, head.authority), (Version::Http10, None));
}

/// A-595: do read versions 1.2, 2.0 and the HTTP/2 preface. Expect a later minor version read as 1.1 and another major
/// version refused 505.
#[test]
fn a_later_minor_version_reads_as_1_1_and_another_major_is_refused() {
  let input = request("GET / HTTP/1.2", &["Host: h"]);
  let (head, _) = whole(&input);
  assert_eq!(head.version, Version::Http11);
  assert_eq!(
    refused(&request("GET / HTTP/2.0", &["Host: h"])),
    HeadRefused::VersionNotSupported
  );
  assert_eq!(
    refused(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"),
    HeadRefused::VersionNotSupported
  );
}

/// A-595: do send empty lines before a request line, and a body after a head. Expect the empty lines skipped and taken,
/// and the head to end at its empty line, leaving the body.
#[test]
fn a_head_skips_empty_lines_before_it_and_ends_at_its_own() {
  let mut input = b"\r\n\r\n".to_vec();
  let head_bytes = request(
    "POST /r/git-receive-pack HTTP/1.1",
    &["Host: h", "Content-Length: 5"],
  );
  input.extend_from_slice(&head_bytes);
  input.extend_from_slice(b"hello");
  let (head, consumed) = whole(&input);
  assert_eq!(head.method, b"POST");
  assert_eq!(consumed, 4 + head_bytes.len());
  assert_eq!(&input[consumed..], b"hello");
}

/// A-595: do read absolute-form targets. Expect the path and query, `/` for no path, and the target's authority over
/// `Host`'s; a userinfo, another scheme or no authority refused.
#[test]
fn an_absolute_form_target_gives_its_path_its_query_and_its_authority() {
  let input = request(
    "GET http://Example.com:8080/r.git/info/refs?service=git-upload-pack HTTP/1.1",
    &["Host: other"],
  );
  let (head, _) = whole(&input);
  assert_eq!(
    head.target,
    Target::Path {
      path: b"/r.git/info/refs",
      query: Some(b"service=git-upload-pack")
    }
  );
  assert_eq!(head.authority, Some(&b"Example.com:8080"[..]));
  let input = request("GET HTTPS://h HTTP/1.1", &["Host: h"]);
  let (head, _) = whole(&input);
  assert_eq!(
    head.target,
    Target::Path {
      path: b"/",
      query: None
    }
  );
  let input = request("GET http://h?a=b HTTP/1.1", &["Host: h"]);
  let (head, _) = whole(&input);
  assert_eq!(
    head.target,
    Target::Path {
      path: b"/",
      query: Some(b"a=b")
    }
  );
  for line in [
    "GET http://user@h/ HTTP/1.1",
    "GET ftp://h/ HTTP/1.1",
    "GET http:/// HTTP/1.1",
    "GET http://h/a#b HTTP/1.1",
  ] {
    assert_eq!(
      refused(&request(line, &["Host: h"])),
      HeadRefused::BadRequest(Malformed::Target),
      "{line}"
    );
  }
}

/// A-595: do ask `OPTIONS *` and `GET *`. Expect the asterisk form for `OPTIONS` alone.
#[test]
fn the_asterisk_form_is_options_alone() {
  let input = request("OPTIONS * HTTP/1.1", &["Host: h"]);
  let (head, _) = whole(&input);
  assert_eq!(head.target, Target::Asterisk);
  assert_eq!(
    refused(&request("GET * HTTP/1.1", &["Host: h"])),
    HeadRefused::BadRequest(Malformed::Target)
  );
}

/// A-595: do send `Expect` with `100-continue` over each version, and another expectation. Expect HTTP/1.1's honoured
/// in any case, HTTP/1.0's ignored, and another refused 417.
#[test]
fn expect_100_continue_is_honoured_for_1_1_ignored_for_1_0_and_another_is_417() {
  let input = request(
    "POST /r/git-upload-pack HTTP/1.1",
    &["Host: h", "Expect: 100-Continue", "Content-Length: 3"],
  );
  let (head, _) = whole(&input);
  assert!(head.expect_continue);
  let input = request(
    "POST /r/git-upload-pack HTTP/1.0",
    &["Expect: 100-continue", "Content-Length: 3"],
  );
  let (head, _) = whole(&input);
  assert!(!head.expect_continue);
  assert_eq!(
    refused(&request("GET / HTTP/1.1", &["Host: h", "Expect: 200-ok"])),
    HeadRefused::ExpectationFailed
  );
}

/// A-595: do read every cut of a head, in a fresh reader each. Expect each cut partial with nothing charged, and the
/// whole head complete.
#[test]
fn every_cut_of_a_head_is_partial_and_charges_nothing() {
  let input = request(
    "POST /r.git/git-receive-pack HTTP/1.1",
    &[
      "Host: h",
      "Content-Type: application/x-git-receive-pack-request",
      "Content-Length: 10",
    ],
  );
  for cut in 0..input.len() {
    let mut admission = Admission::new(ROOM);
    assert_eq!(
      HeadReader::new().read(&input[..cut], &mut admission),
      Ok(Progress::Partial),
      "cut at {cut}"
    );
    assert_eq!(admission.left(), ROOM, "cut at {cut}");
  }
  assert!(matches!(read(&input), Ok(Progress::Complete { .. })));
}

/// A-595: do feed one reader a head a byte more at a time, then two heads one after the other. Expect partial until the
/// last byte, then the same head one read gives; and the reader ready for the next head once it has read one.
#[test]
fn a_head_fed_a_byte_at_a_time_reads_as_one_read_does() {
  let input = request(
    "GET /r.git/info/refs?service=git-receive-pack HTTP/1.1",
    &["Host: h", "Accept: */*", "Pragma: no-cache"],
  );
  let (once, once_consumed) = whole(&input);
  let mut reader = HeadReader::new();
  let mut admission = Admission::new(ROOM);
  for cut in 0..input.len() {
    assert_eq!(
      reader.read(&input[..cut], &mut admission),
      Ok(Progress::Partial),
      "cut at {cut}"
    );
  }
  let Ok(Progress::Complete { value, consumed }) = reader.read(&input, &mut admission) else {
    panic!("not complete");
  };
  assert_eq!((value, consumed), (once, once_consumed));
  let second = request("GET / HTTP/1.1", &["Host: g"]);
  let Ok(Progress::Complete { value, .. }) = reader.read(&second, &mut admission) else {
    panic!("the next head did not read");
  };
  assert_eq!(value.authority, Some(&b"g"[..]));
}

/// A-595: do read a head with three fields under an admission. Expect its bytes and its field index charged exactly,
/// and a room one byte short of that refused 431 with nothing charged.
#[test]
fn a_head_is_charged_its_bytes_and_its_field_index_exactly() {
  let input = request("GET / HTTP/1.1", &["Host: h", "A: 1", "B: 2"]);
  let charge = to_u64(input.len() + 3 * core::mem::size_of::<Field<'_>>());
  let mut admission = Admission::new(charge + 7);
  let read = HeadReader::new().read(&input, &mut admission);
  assert!(matches!(read, Ok(Progress::Complete { .. })), "{read:?}");
  assert_eq!(admission.left(), 7);
  let mut short = Admission::new(charge - 1);
  assert_eq!(
    HeadReader::new().read(&input, &mut short),
    Err(HeadRefused::FieldsTooLarge)
  );
  assert_eq!(short.left(), charge - 1);
}

/// A-595: do read heads past their room. Expect 414 while the request line has not ended in the room, 431 after, and
/// partial while the input is shorter than the room.
#[test]
fn a_head_past_its_room_is_refused_414_or_431() {
  let input = request(
    "GET /a-long-enough-path HTTP/1.1",
    &["Host: h", "X-Long: 0123456789"],
  );
  let line_end = input.windows(2).position(|pair| pair == b"\r\n").unwrap();
  let mut admission = Admission::new(to_u64(line_end));
  assert_eq!(
    HeadReader::new().read(&input, &mut admission),
    Err(HeadRefused::UriTooLong)
  );
  let mut admission = Admission::new(to_u64(line_end + 10));
  assert_eq!(
    HeadReader::new().read(&input, &mut admission),
    Err(HeadRefused::FieldsTooLarge)
  );
  let mut admission = Admission::new(to_u64(input.len() + 100));
  assert_eq!(
    HeadReader::new().read(&input[..line_end + 10], &mut admission),
    Ok(Progress::Partial)
  );
}

/// A-595: do send a request line and a field line at [`MAX_LINE`] and a byte past it, whole and still open. Expect the
/// line at the bound read, the one past it refused (414 for the request line, 431 for a field) without waiting for its
/// end.
#[test]
fn lines_past_max_line_are_refused_whole_or_still_open() {
  let at_bound = format!("GET /{} HTTP/1.1", "a".repeat(MAX_LINE - 14));
  assert_eq!(at_bound.len(), MAX_LINE);
  assert!(matches!(
    read(&request(&at_bound, &["Host: h"])),
    Ok(Progress::Complete { .. })
  ));
  let past = format!("GET /{} HTTP/1.1", "a".repeat(MAX_LINE - 13));
  assert_eq!(
    refused(&request(&past, &["Host: h"])),
    HeadRefused::UriTooLong
  );
  let open = format!("GET /{}", "a".repeat(MAX_LINE + 2));
  assert_eq!(refused(open.as_bytes()), HeadRefused::UriTooLong);
  let field = format!("X: {}", "b".repeat(MAX_LINE - 3));
  assert!(matches!(
    read(&request("GET / HTTP/1.1", &["Host: h", &field])),
    Ok(Progress::Complete { .. })
  ));
  let field = format!("X: {}", "b".repeat(MAX_LINE - 2));
  assert_eq!(
    refused(&request("GET / HTTP/1.1", &["Host: h", &field])),
    HeadRefused::FieldsTooLarge
  );
  let open = format!("GET / HTTP/1.1\r\nHost: h\r\nX: {}", "b".repeat(MAX_LINE));
  assert_eq!(refused(open.as_bytes()), HeadRefused::FieldsTooLarge);
}

/// A-595: do break the request line's and the fields' grammar one way at a time. Expect each refused 400 naming its
/// fault, and HTAB and obs-text in a value read.
#[test]
fn the_grammar_is_held_and_each_break_is_named() {
  let cases: [(&[u8], Malformed); 22] = [
    (
      b"GET  / HTTP/1.1\r\nHost: h\r\n\r\n",
      Malformed::RequestLine,
    ),
    (b"GET /\r\nHost: h\r\n\r\n", Malformed::RequestLine),
    (
      b"GET / HTTP/1.1 x\r\nHost: h\r\n\r\n",
      Malformed::RequestLine,
    ),
    (b"G(T / HTTP/1.1\r\nHost: h\r\n\r\n", Malformed::Method),
    (b"GET / http/1.1\r\nHost: h\r\n\r\n", Malformed::Version),
    (b"GET / HTTP/1.10\r\nHost: h\r\n\r\n", Malformed::Version),
    (
      b"GET relative HTTP/1.1\r\nHost: h\r\n\r\n",
      Malformed::Target,
    ),
    (b"GET /a#b HTTP/1.1\r\nHost: h\r\n\r\n", Malformed::Target),
    (b"GET /%zz HTTP/1.1\r\nHost: h\r\n\r\n", Malformed::Target),
    (b"GET /%4 HTTP/1.1\r\nHost: h\r\n\r\n", Malformed::Target),
    (
      b"GET /\xc3\xa9 HTTP/1.1\r\nHost: h\r\n\r\n",
      Malformed::Target,
    ),
    (b"GET /a\"b HTTP/1.1\r\nHost: h\r\n\r\n", Malformed::Target),
    (b"GET / HTTP/1.1\r\nHost : h\r\n\r\n", Malformed::FieldName),
    (
      b"GET / HTTP/1.1\r\nHost: h\r\nBad Name: x\r\n\r\n",
      Malformed::FieldName,
    ),
    (
      b"GET / HTTP/1.1\r\nHost: h\r\nNoColon\r\n\r\n",
      Malformed::FieldName,
    ),
    (
      b"GET / HTTP/1.1\r\nHost: h\r\n: empty\r\n\r\n",
      Malformed::FieldName,
    ),
    (
      b"GET / HTTP/1.1\r\nHost: h\r\nX: a\r\n folded\r\n\r\n",
      Malformed::Folded,
    ),
    (
      b"GET / HTTP/1.1\r\nHost: h\r\nX: a\x00b\r\n\r\n",
      Malformed::FieldValue,
    ),
    (
      b"GET / HTTP/1.1\r\nHost: h\r\nX: a\x7fb\r\n\r\n",
      Malformed::FieldValue,
    ),
    (
      b"GET / HTTP/1.1\r\nHost: h\r\nX: a\rb\r\n\r\n",
      Malformed::LineBreak,
    ),
    (
      b"GET / HTTP/1.1\r\nHost: h\r\nX: a\nb\r\n\r\n",
      Malformed::LineBreak,
    ),
    (b"GET / HTTP/1.1\nHost: h\r\n\r\n", Malformed::LineBreak),
  ];
  for (input, fault) in cases {
    assert_eq!(
      refused(input),
      HeadRefused::BadRequest(fault),
      "{:?}",
      String::from_utf8_lossy(input)
    );
  }
  let (head, _) = whole(b"GET / HTTP/1.1\r\nHost: h\r\nX: a\tb \xe9\r\n\r\n");
  assert_eq!(head.field(b"x"), Ok(Some(&b"a\tb \xe9"[..])));
}

/// A-595: do send HTTP/1.1 and HTTP/1.0 requests with no, one and two `Host` lines, and bad hosts. Expect `Host`
/// required once in HTTP/1.1, at most once in HTTP/1.0, and an authority with no userinfo.
#[test]
fn host_is_required_once_in_1_1_and_is_an_authority() {
  assert_eq!(
    refused(b"GET / HTTP/1.1\r\n\r\n"),
    HeadRefused::BadRequest(Malformed::NoHost)
  );
  assert_eq!(
    refused(b"GET / HTTP/1.1\r\nHost: a\r\nHost: a\r\n\r\n"),
    HeadRefused::BadRequest(Malformed::RepeatedHost)
  );
  assert_eq!(
    refused(b"GET / HTTP/1.0\r\nHost: a\r\nhost: b\r\n\r\n"),
    HeadRefused::BadRequest(Malformed::RepeatedHost)
  );
  for host in ["a b", "user@h", "h/x", "h?x"] {
    let input = request("GET / HTTP/1.1", &[&format!("Host: {host}")]);
    assert_eq!(
      refused(&input),
      HeadRefused::BadRequest(Malformed::Host),
      "{host}"
    );
  }
  for host in ["[::1]:8080", "127.0.0.1", "xn--bcher-kva.example", ""] {
    let input = request("GET / HTTP/1.1", &[&format!("Host: {host}")]);
    let (head, _) = whole(&input);
    assert_eq!(head.authority, Some(host.as_bytes()), "{host}");
  }
}

/// A-595: do send `Content-Length` values well and badly formed, repeated alike and unlike, and past 64 bits. Expect
/// digits alone read, a list or repeat of one value read as it (RFC 9112 §6.3 item 5), unlike values and anything not
/// digits refused 400, and a value past 64 bits 413.
#[test]
fn content_length_is_digits_and_its_values_agree() {
  let line = "POST /r/git-upload-pack HTTP/1.1";
  let good: [(&[&str], u64); 4] = [
    (&["Content-Length: 0"], 0),
    (&["Content-Length: 5, 5"], 5),
    (&["Content-Length: 5", "content-length: 5"], 5),
    (&["Content-Length: 18446744073709551615"], u64::MAX),
  ];
  for (fields, length) in good {
    let mut all = vec!["Host: h"];
    all.extend_from_slice(fields);
    let input = request(line, &all);
    let (head, _) = whole(&input);
    assert_eq!(head.framing, Framing::Length(length), "{fields:?}");
  }
  let bad: [&[&str]; 7] = [
    &["Content-Length: +5"],
    &["Content-Length: 5x"],
    &["Content-Length: -1"],
    &["Content-Length:"],
    &["Content-Length: 0x10"],
    &["Content-Length: 5, 6"],
    &["Content-Length: 5", "Content-Length: 6"],
  ];
  for fields in bad {
    let mut all = vec!["Host: h"];
    all.extend_from_slice(fields);
    assert_eq!(
      refused(&request(line, &all)),
      HeadRefused::BadRequest(Malformed::ContentLength),
      "{fields:?}"
    );
  }
  assert_eq!(
    refused(&request(
      line,
      &["Host: h", "Content-Length: 18446744073709551616"]
    )),
    HeadRefused::ContentTooLarge
  );
}

/// A-595: do send every framing two HTTP readers could disagree on. Expect each refused by its kind, 400 or 501.
#[test]
fn framing_two_readers_could_disagree_on_is_refused() {
  let line = "POST /r/git-receive-pack HTTP/1.1";
  let cases: [(&[&str], HeadRefused); 7] = [
    (
      &["Content-Length: 5", "Transfer-Encoding: chunked"],
      HeadRefused::BadRequest(Malformed::LengthAndTransferCoding),
    ),
    (
      &["Transfer-Encoding: gzip, chunked"],
      HeadRefused::NotImplemented,
    ),
    (
      &["Transfer-Encoding: chunked, gzip"],
      HeadRefused::BadRequest(Malformed::ChunkedNotLast),
    ),
    (
      &["Transfer-Encoding: chunked, chunked"],
      HeadRefused::BadRequest(Malformed::ChunkedTwice),
    ),
    (
      &["Transfer-Encoding: chunked", "Transfer-Encoding: chunked"],
      HeadRefused::BadRequest(Malformed::ChunkedTwice),
    ),
    (
      &["Transfer-Encoding:"],
      HeadRefused::BadRequest(Malformed::ChunkedNotLast),
    ),
    (
      &["Transfer-Encoding: gzip", "Transfer-Encoding: chunked"],
      HeadRefused::NotImplemented,
    ),
  ];
  for (fields, refusal) in cases {
    let mut all = vec!["Host: h"];
    all.extend_from_slice(fields);
    assert_eq!(refused(&request(line, &all)), refusal, "{fields:?}");
  }
  assert_eq!(
    refused(&request(
      "POST /r/git-receive-pack HTTP/1.0",
      &["Transfer-Encoding: chunked"]
    )),
    HeadRefused::BadRequest(Malformed::TransferCodingInHttp10)
  );
}

/// A-595: do send content codings other than one gzip. Expect each refused 415, and gzip with identity read as gzip.
#[test]
fn content_codings_other_than_one_gzip_are_refused_415() {
  let line = "POST /r/git-upload-pack HTTP/1.1";
  for coding in ["br", "deflate", "gzip, gzip", "gzip;q=1"] {
    let input = request(
      line,
      &[
        "Host: h",
        "Content-Length: 1",
        &format!("Content-Encoding: {coding}"),
      ],
    );
    assert_eq!(refused(&input), HeadRefused::UnsupportedCoding, "{coding}");
  }
  let input = request(
    line,
    &[
      "Host: h",
      "Content-Length: 1",
      "Content-Encoding: gzip, identity",
    ],
  );
  let (head, _) = whole(&input);
  assert_eq!(head.coding, Coding::Gzip);
}

/// A-595: do send `CONNECT`. Expect 501: no server here tunnels.
#[test]
fn connect_is_not_implemented() {
  assert_eq!(
    refused(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n"),
    HeadRefused::NotImplemented
  );
}

/// A-595: do send a head with several faults, its fields in one order and then reversed. Expect the same refusal both
/// times, the first in the fixed order: `Host`, the length, the framing, the coding, the expectation.
#[test]
fn refusals_are_judged_in_one_order_whatever_the_fields_order() {
  let fields = [
    "Expect: nothing",
    "Content-Encoding: br",
    "Content-Length: x",
    "Host: h",
  ];
  let mut reversed = fields;
  reversed.reverse();
  for order in [fields, reversed] {
    assert_eq!(
      refused(&request("POST / HTTP/1.1", &order)),
      HeadRefused::BadRequest(Malformed::ContentLength),
      "{order:?}"
    );
  }
  let fields = ["Expect: nothing", "Content-Encoding: br", "Host: h"];
  assert_eq!(
    refused(&request("POST / HTTP/1.1", &fields)),
    HeadRefused::UnsupportedCoding
  );
}

/// A-595: do ask for one value of a field sent twice, and for all of them. Expect the one refused as ambiguous, and both
/// in their order.
#[test]
fn a_field_asked_for_once_but_sent_twice_is_refused() {
  let input = request(
    "POST / HTTP/1.1",
    &[
      "Host: h",
      "Content-Type: a/b",
      "content-type: c/d",
      "Content-Length: 0",
    ],
  );
  let (head, _) = whole(&input);
  assert_eq!(
    head.field(b"Content-Type"),
    Err(HeadRefused::BadRequest(Malformed::RepeatedField))
  );
  assert_eq!(
    head.fields_named(b"CONTENT-TYPE").collect::<Vec<_>>(),
    vec![&b"a/b"[..], &b"c/d"[..]]
  );
}

/// A-595: do take media types' essences. Expect the type and subtype without parameters or whitespace.
#[test]
fn a_media_type_is_its_essence() {
  assert_eq!(
    media_type(b"application/x-git-upload-pack-request"),
    b"application/x-git-upload-pack-request"
  );
  assert_eq!(media_type(b" text/plain ; charset=utf-8"), b"text/plain");
  assert_eq!(media_type(b""), b"");
}

/// A-595: do ask each refusal's status and reply. Expect RFC 9110's code for each, every reply closing the connection
/// with no body, and a refused coding naming gzip.
#[test]
fn each_refusal_has_its_status_and_a_closing_reply() {
  let cases: [(HeadRefused, u16); 8] = [
    (HeadRefused::BadRequest(Malformed::NoHost), 400),
    (HeadRefused::ContentTooLarge, 413),
    (HeadRefused::UriTooLong, 414),
    (HeadRefused::UnsupportedCoding, 415),
    (HeadRefused::ExpectationFailed, 417),
    (HeadRefused::FieldsTooLarge, 431),
    (HeadRefused::NotImplemented, 501),
    (HeadRefused::VersionNotSupported, 505),
  ];
  for (refusal, code) in cases {
    assert_eq!(refusal.status().code, code, "{refusal:?}");
  }
  let mut out = Vec::new();
  HeadRefused::UnsupportedCoding.reply(&mut out).unwrap();
  assert_eq!(
    out,
    b"HTTP/1.1 415 Unsupported Media Type\r\nAccept-Encoding: gzip\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
  );
  let mut out = Vec::new();
  HeadRefused::UriTooLong.reply(&mut out).unwrap();
  assert_eq!(
    out,
    b"HTTP/1.1 414 URI Too Long\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
  );
  assert_eq!(
    HeadRefused::FieldsTooLarge.status(),
    Status::FIELDS_TOO_LARGE
  );
}
