use std::io::Write as _;

use super::*;

/// A head's bytes from its lines, each ended by CRLF (the caller hands the head without its blank line).
fn head(lines: &[&str]) -> Vec<u8> {
  lines.join("\r\n").into_bytes()
}

/// A gzip stream of `data`, as git's client compresses a long request.
fn gzip(data: &[u8]) -> Vec<u8> {
  let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
  encoder.write_all(data).unwrap();
  encoder.finish().unwrap()
}

/// A-595. Do: parse an advertisement's GET with `Git-Protocol: version=2`, a POST framed by `Content-Length` and
/// gzip-encoded, a chunked POST, and an HTTP/1.0 request and one asking to close. Expect each field git's routes read,
/// each framing, and the connection's keep as HTTP/1.1, HTTP/1.0 and `Connection` say.
#[test]
fn a_request_head_is_parsed_with_gits_fields_and_its_framing() {
  let get = parse_head(&head(&[
    "GET /team/repo.git/info/refs?service=git-upload-pack HTTP/1.1",
    "Host: localhost:8080",
    "Git-Protocol: version=2",
    "Accept: */*",
  ]))
  .unwrap();
  assert_eq!(get.method, Method::Get);
  assert_eq!(
    (get.path.as_str(), get.query.as_str()),
    ("/team/repo.git/info/refs", "service=git-upload-pack")
  );
  assert_eq!(get.git_protocol.as_deref(), Some("version=2"));
  assert_eq!(
    (get.framing, get.keep_alive, get.gzip),
    (Framing::None, true, false)
  );
  let post = parse_head(&head(&[
    "POST /repo.git/git-upload-pack HTTP/1.1",
    "Host: localhost",
    "Content-Type: application/x-git-upload-pack-request",
    "Content-Encoding: gzip",
    "Content-Length: 1234",
    "Authorization: Basic dXNlcjpwYXNz",
  ]))
  .unwrap();
  assert_eq!(
    (post.method, post.framing, post.gzip),
    (Method::Post, Framing::Length(1234), true)
  );
  assert_eq!(
    post.content_type.as_deref(),
    Some("application/x-git-upload-pack-request")
  );
  assert_eq!(post.authorization.as_deref(), Some("Basic dXNlcjpwYXNz"));
  let chunked = parse_head(&head(&[
    "POST /repo.git/git-receive-pack HTTP/1.1",
    "Host: h",
    "Transfer-Encoding: chunked",
  ]))
  .unwrap();
  assert_eq!(chunked.framing, Framing::Chunked);
  assert!(
    !parse_head(&head(&["GET /r/info/refs HTTP/1.0"]))
      .unwrap()
      .keep_alive,
    "HTTP/1.0 closes"
  );
  assert!(
    !parse_head(&head(&[
      "GET /r/info/refs HTTP/1.1",
      "Host: h",
      "Connection: close"
    ]))
    .unwrap()
    .keep_alive
  );
}

/// A-595 (hostile input, §4.9). Do: parse heads past their bounds and heads that contradict themselves. Expect each
/// refused by its kind: a request line past [`MAX_LINE`] 414; a field line past it, or more than [`MAX_FIELDS`] fields,
/// 431; both framings, two lengths that differ, an HTTP/1.1 request without a `Host`, two `Host`s, a space before a
/// colon, a length that isn't a number, or another version 400; a transfer coding other than chunked 501; a content
/// coding other than gzip 415.
#[test]
fn a_head_past_its_bounds_or_contradicting_itself_is_refused_by_kind() {
  let long_target = format!("GET /{} HTTP/1.1", "a".repeat(MAX_LINE));
  assert_eq!(parse_head(long_target.as_bytes()), Err(Refused::UriTooLong));
  let long_field = format!("X-Long: {}", "b".repeat(MAX_LINE));
  assert_eq!(
    parse_head(&head(&[
      "GET /r/info/refs HTTP/1.1",
      "Host: h",
      &long_field
    ])),
    Err(Refused::HeadersTooLarge)
  );
  let mut many = vec!["GET /r/info/refs HTTP/1.1".to_owned(), "Host: h".to_owned()];
  many.extend((0..MAX_FIELDS).map(|n| format!("X-{n}: v")));
  let many: Vec<&str> = many.iter().map(String::as_str).collect();
  assert_eq!(parse_head(&head(&many)), Err(Refused::HeadersTooLarge));
  for (lines, refused) in [
    (
      vec![
        "POST /r/git-upload-pack HTTP/1.1",
        "Host: h",
        "Content-Length: 5",
        "Transfer-Encoding: chunked",
      ],
      Refused::BadRequest,
    ),
    (
      vec![
        "POST /r/git-upload-pack HTTP/1.1",
        "Host: h",
        "Content-Length: 5",
        "Content-Length: 6",
      ],
      Refused::BadRequest,
    ),
    (vec!["GET /r/info/refs HTTP/1.1"], Refused::BadRequest),
    (
      vec!["GET /r/info/refs HTTP/1.1", "Host: a", "Host: b"],
      Refused::BadRequest,
    ),
    (
      vec!["GET /r/info/refs HTTP/1.1", "Host : h"],
      Refused::BadRequest,
    ),
    (
      vec![
        "POST /r/git-upload-pack HTTP/1.1",
        "Host: h",
        "Content-Length: five",
      ],
      Refused::BadRequest,
    ),
    (
      vec!["GET /r/info/refs HTTP/2.0", "Host: h"],
      Refused::BadRequest,
    ),
    (
      vec!["GET r/info/refs HTTP/1.1", "Host: h"],
      Refused::BadRequest,
    ),
    (
      vec![
        "POST /r/git-upload-pack HTTP/1.1",
        "Host: h",
        "Transfer-Encoding: gzip, chunked",
      ],
      Refused::NotImplemented,
    ),
    (
      vec![
        "POST /r/git-upload-pack HTTP/1.1",
        "Host: h",
        "Content-Length: 5",
        "Content-Encoding: br",
      ],
      Refused::UnsupportedMediaType,
    ),
  ] {
    assert_eq!(parse_head(&head(&lines)), Err(refused), "{lines:?}");
  }
  assert_eq!(
    parse_head(&[0xff, 0xfe]),
    Err(Refused::BadRequest),
    "a head that isn't text"
  );
}

/// A chunked body with an extension and a trailer, then a byte of the next request: what every chunked test reads.
const CHUNKED: &[u8] = b"5;name=value\r\nhello\r\n7\r\n, world\r\n0\r\nX-Trailer: t\r\n\r\nN";

/// A-595. Do: decode a chunked body with a chunk extension and a trailer, followed by a byte of the next request; then
/// every cut of it short of its end. Expect the body's data joined, the bytes it took ending before the next request's,
/// its exact size charged; and every cut [`Chunked::Partial`], charging nothing (a reader waits for more).
#[test]
fn a_chunked_body_decodes_whole_and_every_cut_of_it_is_partial() {
  let mut admission = Admission::new(1000);
  assert_eq!(
    decode_chunked(CHUNKED, &mut admission),
    Ok(Chunked::Whole {
      body: b"hello, world".to_vec(),
      consumed: CHUNKED.len() - 1,
    })
  );
  assert_eq!(admission.left(), 1000 - 12, "the body's exact size charged");
  for cut in 0..CHUNKED.len() - 1 {
    let mut room = Admission::new(1000);
    assert_eq!(
      decode_chunked(&CHUNKED[..cut], &mut room),
      Ok(Chunked::Partial),
      "cut at {cut}"
    );
    assert_eq!(room.left(), 1000, "a cut charges nothing");
  }
}

/// A-595 (hostile input). Do: decode chunked bodies whose size lines aren't hexadecimal, are empty, overflow 64 bits or
/// run past [`MAX_LINE`]; one whose chunk data isn't followed by its CRLF; and one with more trailer lines than
/// [`MAX_FIELDS`]. Expect each refused [`BodyRefused::BadChunk`], never a panic or a partial wait.
#[test]
fn bad_chunk_sizes_and_framing_are_refused() {
  let long_size = format!("{}\r\n", "1".repeat(MAX_LINE + 2));
  let mut trailers = String::from("0\r\n");
  trailers.push_str(&"X: y\r\n".repeat(MAX_FIELDS + 1));
  trailers.push_str("\r\n");
  for body in [
    b"zz\r\nhello\r\n0\r\n\r\n".to_vec(),
    b"\r\nhello\r\n0\r\n\r\n".to_vec(),
    b"10000000000000000\r\n".to_vec(),
    b"-5\r\nhello\r\n0\r\n\r\n".to_vec(),
    b"+5\r\nhello\r\n0\r\n\r\n".to_vec(),
    long_size.into_bytes(),
    b"5\r\nhelloXX0\r\n\r\n".to_vec(),
    trailers.into_bytes(),
  ] {
    assert_eq!(
      decode_chunked(&body, &mut Admission::new(u64::MAX)),
      Err(BodyRefused::BadChunk),
      "{:?}",
      String::from_utf8_lossy(&body)
    );
  }
}

/// A-595 (hostile input). Do: decode a chunked body whose first chunk declares four gigabytes, none of it sent, under an
/// admission of a kilobyte. Expect it refused past the admission at once, not waited for, and nothing charged.
#[test]
fn a_chunked_body_declaring_past_its_admission_is_refused_before_its_data_arrives() {
  let mut admission = Admission::new(1024);
  assert_eq!(
    decode_chunked(b"ffffffff\r\n", &mut admission),
    Err(BodyRefused::TooLarge(PastAdmission {
      asked: 0xffff_ffff,
      left: 1024,
    }))
  );
  assert_eq!(admission.left(), 1024);
}

/// A-595 (hostile input). Do: inflate a gzip body within its admission; a gzip bomb, a megabyte of zeros in about a
/// kilobyte, under an admission of 64 KiB; bytes that aren't gzip; and every cut of the good body. Expect the first
/// inflated whole and its size charged; the bomb refused past its admission having made no more than the room and one
/// byte; the others refused [`BodyRefused::BadGzip`], never inflated in part.
#[test]
fn a_gzip_body_inflates_within_its_admission_and_a_bomb_stops_at_it() {
  let text = b"0032want 0123456789012345678901234567890123456789\n0000".repeat(40);
  let packed = gzip(&text);
  let mut admission = Admission::new(1 << 20);
  assert_eq!(
    gunzip(&packed, &mut admission).as_deref(),
    Ok(text.as_slice())
  );
  assert_eq!(
    admission.left(),
    (1 << 20) - u64::try_from(text.len()).unwrap()
  );
  let bomb = gzip(&vec![0u8; 1 << 20]);
  assert!(bomb.len() < 4096, "a bomb: {} bytes", bomb.len());
  let mut room = Admission::new(1 << 16);
  assert_eq!(
    gunzip(&bomb, &mut room),
    Err(BodyRefused::TooLarge(PastAdmission {
      asked: (1 << 16) + 1,
      left: 1 << 16,
    }))
  );
  assert_eq!(room.left(), 1 << 16, "a refused inflate charges nothing");
  assert_eq!(
    gunzip(b"not gzip at all", &mut Admission::new(1 << 20)),
    Err(BodyRefused::BadGzip)
  );
  for cut in 0..packed.len() {
    assert_eq!(
      gunzip(&packed[..cut], &mut Admission::new(1 << 20)),
      Err(BodyRefused::BadGzip),
      "cut at {cut}"
    );
  }
}

/// The head a route test asks with: `method target`, `Host`, and `extra` fields.
fn asked(method_and_target: &str, extra: &[&str]) -> RequestHead {
  let mut lines = vec![
    format!("{method_and_target} HTTP/1.1"),
    "Host: h".to_owned(),
  ];
  lines.extend(extra.iter().map(|line| (*line).to_owned()));
  let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
  parse_head(&head(&lines)).unwrap()
}

/// A-595. Do: route an advertisement's GET for each service with and without `Git-Protocol: version=2`, each service's
/// POST of its content type with a body, and requests no route takes. Expect the advertisement and the service with the
/// version asked; and each other refused by its kind: no service or an unknown one 403, another method 405, another
/// content type 415, no body 411, a path leaving its repository 400, and a path no route names 404.
#[test]
fn routes_name_the_advertisement_and_the_services_and_refuse_the_rest_by_kind() {
  let repo = || "/team/repo.git".to_owned();
  assert_eq!(
    route(&asked(
      "GET /team/repo.git/info/refs?service=git-upload-pack",
      &[]
    )),
    Ok(Route::Advertise {
      repo: repo(),
      service: Service::UploadPack,
      version: Version::V0
    })
  );
  assert_eq!(
    route(&asked(
      "GET /team/repo.git/info/refs?service=git-receive-pack",
      &["Git-Protocol: version=2"]
    )),
    Ok(Route::Advertise {
      repo: repo(),
      service: Service::ReceivePack,
      version: Version::V2
    })
  );
  let request = |service: &str| format!("Content-Type: application/x-{service}-request");
  assert_eq!(
    route(&asked(
      "POST /team/repo.git/git-upload-pack",
      &[
        &request("git-upload-pack"),
        "Content-Length: 4",
        "Git-Protocol: version=2"
      ]
    )),
    Ok(Route::Serve {
      repo: repo(),
      service: Service::UploadPack,
      version: Version::V2
    })
  );
  assert_eq!(
    route(&asked(
      "POST /team/repo.git/git-receive-pack",
      &[&request("git-receive-pack"), "Transfer-Encoding: chunked"]
    )),
    Ok(Route::Serve {
      repo: repo(),
      service: Service::ReceivePack,
      version: Version::V0
    })
  );
  for (head, refused) in [
    (asked("GET /r.git/info/refs", &[]), Refused::Forbidden),
    (
      asked("GET /r.git/info/refs?service=git-annex", &[]),
      Refused::Forbidden,
    ),
    (
      asked(
        "POST /r.git/info/refs?service=git-upload-pack",
        &["Content-Length: 0"],
      ),
      Refused::MethodNotAllowed,
    ),
    (
      asked("GET /r.git/git-upload-pack", &[]),
      Refused::MethodNotAllowed,
    ),
    (
      asked(
        "POST /r.git/git-upload-pack",
        &[&request("git-receive-pack"), "Content-Length: 4"],
      ),
      Refused::UnsupportedMediaType,
    ),
    (
      asked(
        "POST /r.git/git-upload-pack",
        &[&request("git-upload-pack")],
      ),
      Refused::LengthRequired,
    ),
    (
      asked(
        "GET /r.git/../other.git/info/refs?service=git-upload-pack",
        &[],
      ),
      Refused::BadRequest,
    ),
    (
      asked("GET //info/refs?service=git-upload-pack", &[]),
      Refused::BadRequest,
    ),
    (
      asked("GET /info/refs?service=git-upload-pack", &[]),
      Refused::NotFound,
    ),
    (asked("GET /r.git/HEAD", &[]), Refused::NotFound),
  ] {
    assert_eq!(
      route(&head),
      Err(refused),
      "{} {}?{}",
      match &head.method {
        Method::Get => "GET",
        Method::Post => "POST",
        Method::Other(m) => m,
      },
      head.path,
      head.query
    );
  }
}

/// A-595. Do: read `Git-Protocol` values: none, `version=2`, `version=1`, keys around it, the last of two, and a version
/// slates doesn't speak. Expect v0, v2, v1, v2, the last asked, and v0.
#[test]
fn the_version_is_the_last_version_key_git_protocol_names() {
  assert_eq!(version(None), Version::V0);
  assert_eq!(version(Some("version=2")), Version::V2);
  assert_eq!(version(Some("version=1")), Version::V1);
  assert_eq!(
    version(Some("object-format=sha1:version=2:agent=git")),
    Version::V2
  );
  assert_eq!(version(Some("version=2:version=1")), Version::V1);
  assert_eq!(version(Some("version=3")), Version::V0);
}

/// A-595. Do: write each service's advertisement prelude for v0, v1 and v2, a reply's head, chunks and refusals. Expect
/// for v0 and v1 the pkt-line `# service=<service>` and a flush, byte for byte as gitprotocol-http writes them, and for v2
/// nothing; a reply with its content type, `Cache-Control: no-cache` and chunked; a chunk as its size in hexadecimal and
/// its data, none for no data; and a refusal's status, a `405` naming the methods the routes take.
#[test]
fn the_advertisement_starts_with_its_service_line_and_a_flush_for_v0_and_v1_and_nothing_for_v2() {
  assert_eq!(
    advertisement_prelude(Service::UploadPack, Version::V0).unwrap(),
    b"001e# service=git-upload-pack\n0000"
  );
  assert_eq!(
    advertisement_prelude(Service::ReceivePack, Version::V1).unwrap(),
    b"001f# service=git-receive-pack\n0000"
  );
  assert_eq!(
    advertisement_prelude(Service::UploadPack, Version::V2).unwrap(),
    b""
  );
  let reply =
    String::from_utf8(reply_head(&advertisement_type(Service::UploadPack), true)).unwrap();
  assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply}");
  for field in [
    "Content-Type: application/x-git-upload-pack-advertisement\r\n",
    "Cache-Control: no-cache\r\n",
    "Transfer-Encoding: chunked\r\n",
  ] {
    assert!(reply.contains(field), "{field:?} in {reply}");
  }
  assert!(
    String::from_utf8(reply_head(&result_type(Service::ReceivePack), false))
      .unwrap()
      .contains("Connection: close\r\n")
  );
  assert_eq!(chunk(b"hello, world"), b"c\r\nhello, world\r\n");
  assert_eq!(chunk(b""), b"");
  assert_eq!(LAST_CHUNK, b"0\r\n\r\n");
  let refused = String::from_utf8(refusal_reply(Refused::MethodNotAllowed)).unwrap();
  assert!(
    refused.starts_with("HTTP/1.1 405 Method Not Allowed\r\n")
      && refused.contains("Allow: GET, POST\r\n"),
    "{refused}"
  );
  assert!(
    String::from_utf8(refusal_reply(Refused::NotFound))
      .unwrap()
      .starts_with("HTTP/1.1 404 Not Found\r\n")
  );
}
