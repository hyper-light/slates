use slates_http::Admission;
use slates_http::head::{Head, HeadReader, HeadRefused, Malformed, Progress, Version as HttpVersion};

use super::{
  Allowed, Reply, Route, RouteRefused, Service, Version, advertisement_prelude, refusal_reply,
  reply_head, route, version,
};
use crate::to_u64;

/// Room enough for every head and path these tests route.
const ROOM: u64 = 1 << 20;

/// The head `input` holds; it must be whole.
fn head(input: &[u8]) -> Head<'_> {
  match HeadReader::new().read(input, &mut Admission::new(ROOM)) {
    Ok(Progress::Complete { value, .. }) => value,
    other => panic!("{:?} read {other:?}", String::from_utf8_lossy(input)),
  }
}

/// `method target` over HTTP/1.1 with `Host` and `fields`, routed.
fn routed(method: &str, target: &str, fields: &[&str]) -> Result<Route, RouteRefused> {
  routed_over("HTTP/1.1", method, target, fields)
}

/// `method target` over `http` with `Host` and `fields`, routed.
fn routed_over(http: &str, method: &str, target: &str, fields: &[&str]) -> Result<Route, RouteRefused> {
  let mut text = format!("{method} {target} {http}\r\nHost: git.example\r\n");
  for field in fields {
    text.push_str(field);
    text.push_str("\r\n");
  }
  text.push_str("\r\n");
  let bytes = text.into_bytes();
  route(&head(&bytes), &mut Admission::new(ROOM))
}

/// The fields of a request to `service`: its content type and a body of ten bytes.
fn service_fields(service: &str) -> [String; 2] {
  [
    format!("Content-Type: application/x-{service}-request"),
    "Content-Length: 10".to_owned(),
  ]
}

/// A-595: do ask for each service's advertisement by `GET` and `HEAD`, with and without `Git-Protocol`. Expect the
/// advertisement of that service and repository at the version asked, the head alone for `HEAD`.
#[test]
fn info_refs_with_a_service_is_its_advertisement() {
  assert_eq!(
    routed("GET", "/team/repo.git/info/refs?service=git-upload-pack", &[]),
    Ok(Route::Advertise {
      repo: "team/repo.git".to_owned(),
      service: Service::UploadPack,
      version: Version::V0,
      head_only: false,
    })
  );
  assert_eq!(
    routed("HEAD", "/repo/info/refs?service=git-receive-pack", &["Git-Protocol: version=2"]),
    Ok(Route::Advertise {
      repo: "repo".to_owned(),
      service: Service::ReceivePack,
      version: Version::V2,
      head_only: true,
    })
  );
  assert_eq!(
    routed("GET", "/r/info/refs?service=git-upload-pack", &["git-protocol: version=1"]),
    Ok(Route::Advertise {
      repo: "r".to_owned(),
      service: Service::UploadPack,
      version: Version::V1,
      head_only: false,
    })
  );
}

/// A-595: do read `Git-Protocol` values. Expect the greatest version git speaks among the `version=` keys, as
/// `protocol.c` chooses it, v0 without one, an unknown version ignored.
#[test]
fn git_protocol_asks_for_its_greatest_version() {
  let cases: [(Option<&[u8]>, Version); 10] = [
    (None, Version::V0),
    (Some(b""), Version::V0),
    (Some(b"version=2"), Version::V2),
    (Some(b"version=1"), Version::V1),
    (Some(b"version=0"), Version::V0),
    (Some(b"version=3"), Version::V0),
    (Some(b"version=22"), Version::V0),
    (Some(b"version=2:version=1"), Version::V2),
    (Some(b"version=1:version=2"), Version::V2),
    (Some(b"object-format=sha256:version=1"), Version::V1),
  ];
  for (value, expected) in cases {
    assert_eq!(version(value), expected, "{value:?}");
  }
}

/// A-595: do POST each service's request framed each way. Expect the service's route, at the version asked.
#[test]
fn a_service_post_of_its_type_with_a_body_is_served() {
  let [content_type, length] = service_fields("git-receive-pack");
  assert_eq!(
    routed("POST", "/a/b.git/git-receive-pack", &[&content_type, &length]),
    Ok(Route::Serve {
      repo: "a/b.git".to_owned(),
      service: Service::ReceivePack,
      version: Version::V0,
    })
  );
  let [content_type, _] = service_fields("git-upload-pack");
  assert_eq!(
    routed(
      "POST",
      "/r.git/git-upload-pack",
      &[&content_type, "Transfer-Encoding: chunked", "Git-Protocol: version=2"]
    ),
    Ok(Route::Serve {
      repo: "r.git".to_owned(),
      service: Service::UploadPack,
      version: Version::V2,
    })
  );
  assert_eq!(
    routed(
      "POST",
      "/r.git/git-upload-pack",
      &["Content-Type: Application/X-Git-Upload-Pack-Request; charset=x", "Content-Length: 4"]
    ),
    Ok(Route::Serve {
      repo: "r.git".to_owned(),
      service: Service::UploadPack,
      version: Version::V0,
    })
  );
}

/// A-595: do send each request `http-backend` refuses. Expect each refused by its kind, with its status.
#[test]
fn every_refusal_is_typed_with_http_backends_status() {
  let [upload_type, upload_length] = service_fields("git-upload-pack");
  let cases: Vec<(&str, &str, Vec<&str>, RouteRefused, u16)> = vec![
    ("GET", "/r/info/refs", vec![], RouteRefused::DumbProtocol, 403),
    ("GET", "/r/info/refs?other=1", vec![], RouteRefused::DumbProtocol, 403),
    ("GET", "/r/info/refs?service=git-frobnicate", vec![], RouteRefused::UnsupportedService, 403),
    ("GET", "/r/info/refs?service=upload-pack", vec![], RouteRefused::UnsupportedService, 403),
    ("GET", "/r/info/refs?service=git-upload-archive", vec![], RouteRefused::ServiceNotServed, 403),
    ("POST", "/r/git-upload-archive", vec![], RouteRefused::ServiceNotServed, 403),
    ("GET", "/r/HEAD", vec![], RouteRefused::DumbProtocol, 403),
    ("GET", "/r/objects/info/packs", vec![], RouteRefused::DumbProtocol, 403),
    ("GET", "/r/objects/info/alternates", vec![], RouteRefused::DumbProtocol, 403),
    ("GET", "/r/objects/info/http-alternates", vec![], RouteRefused::DumbProtocol, 403),
    ("GET", "/r/objects/ab/0123456789abcdef0123456789abcdef012345", vec![], RouteRefused::DumbProtocol, 403),
    (
      "GET",
      "/r/objects/ab/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd",
      vec![],
      RouteRefused::DumbProtocol,
      403,
    ),
    (
      "GET",
      "/r/objects/pack/pack-0123456789abcdef0123456789abcdef01234567.pack",
      vec![],
      RouteRefused::DumbProtocol,
      403,
    ),
    (
      "GET",
      "/r/objects/pack/pack-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.idx",
      vec![],
      RouteRefused::DumbProtocol,
      403,
    ),
    ("GET", "/r/objects/AB/0123456789abcdef0123456789abcdef012345", vec![], RouteRefused::NotFound, 404),
    ("GET", "/r/objects/ab/0123", vec![], RouteRefused::NotFound, 404),
    ("GET", "/r/something-else", vec![], RouteRefused::NotFound, 404),
    ("GET", "/info/refs?service=git-upload-pack", vec![], RouteRefused::NotFound, 404),
    ("GET", "/a/../b/info/refs?service=git-upload-pack", vec![], RouteRefused::NotFound, 404),
    ("GET", "/a/./b/info/refs?service=git-upload-pack", vec![], RouteRefused::NotFound, 404),
    ("GET", "/a//b/info/refs?service=git-upload-pack", vec![], RouteRefused::NotFound, 404),
    ("GET", "/a/%2e%2e/info/refs?service=git-upload-pack", vec![], RouteRefused::NotFound, 404),
    ("GET", "/a%2Fb/info/refs?service=git-upload-pack", vec![], RouteRefused::BadPath, 400),
    ("GET", "/a%00b/info/refs?service=git-upload-pack", vec![], RouteRefused::BadPath, 400),
    ("GET", "/a%ff/info/refs?service=git-upload-pack", vec![], RouteRefused::BadPath, 400),
    ("POST", "/r/info/refs?service=git-upload-pack", vec![], RouteRefused::MethodNotAllowed(Allowed::GetHead), 405),
    ("PUT", "/r/HEAD", vec![], RouteRefused::MethodNotAllowed(Allowed::GetHead), 405),
    ("GET", "/r/git-upload-pack", vec![], RouteRefused::MethodNotAllowed(Allowed::Post), 405),
    ("HEAD", "/r/git-receive-pack", vec![], RouteRefused::MethodNotAllowed(Allowed::Post), 405),
    ("POST", "/r/git-upload-pack", vec![upload_length.as_str()], RouteRefused::UnsupportedMediaType, 415),
    (
      "POST",
      "/r/git-upload-pack",
      vec!["Content-Type: application/x-git-receive-pack-request", upload_length.as_str()],
      UnsupportedMediaType,
      415,
    ),
    ("POST", "/r/git-upload-pack", vec![upload_type.as_str()], RouteRefused::LengthRequired, 411),
    (
      "GET",
      "/r/info/refs?service=git-upload-pack",
      vec!["Git-Protocol: version=2", "Git-Protocol: version=1"],
      RouteRefused::Head(HeadRefused::BadRequest(Malformed::RepeatedField)),
      400,
    ),
    (
      "POST",
      "/r/git-upload-pack",
      vec![upload_type.as_str(), upload_type.as_str(), upload_length.as_str()],
      RouteRefused::Head(HeadRefused::BadRequest(Malformed::RepeatedField)),
      400,
    ),
  ];
  for (method, target, fields, refusal, code) in cases {
    assert_eq!(routed(method, target, &fields), Err(refusal), "{method} {target} {fields:?}");
    assert_eq!(refusal.status(HttpVersion::Http11).code, code, "{refusal:?}");
  }
  assert_eq!(
    routed("OPTIONS", "*", &[]),
    Err(RouteRefused::NotFound)
  );
}

/// A-595: do route requests in the order `http-backend` checks them: a wrong method on an alias path, an alias path for
/// an unknown service, and an unknown service with a wrong content type. Expect the method first, then the path, then
/// the service, then the content type.
#[test]
fn checks_run_in_http_backends_order() {
  assert_eq!(
    routed("GET", "/a/../b/git-upload-pack", &[]),
    Err(RouteRefused::MethodNotAllowed(Allowed::Post))
  );
  assert_eq!(
    routed("GET", "/a/../b/info/refs?service=git-frobnicate", &[]),
    Err(RouteRefused::NotFound)
  );
  assert_eq!(
    routed("POST", "/r/git-upload-archive", &["Content-Type: text/plain"]),
    Err(RouteRefused::ServiceNotServed)
  );
}

/// A-595: do send a `405` to HTTP/1.0 and HTTP/1.1. Expect `400` to HTTP/1.0, which has no `405`, as `bad_request`.
#[test]
fn a_wrong_method_is_400_to_http_1_0() {
  let refused = routed_over("HTTP/1.0", "POST", "/r/info/refs?service=git-upload-pack", &[]);
  assert_eq!(refused, Err(RouteRefused::MethodNotAllowed(Allowed::GetHead)));
  let refusal = RouteRefused::MethodNotAllowed(Allowed::GetHead);
  assert_eq!(refusal.status(HttpVersion::Http10).code, 400);
  assert_eq!(refusal.status(HttpVersion::Http11).code, 405);
}

/// A-595: do route paths and queries with escapes, a `+` and repeated services. Expect the path decoded, the query read
/// as a form, and the last `service` winning, as `http-backend`'s `get_parameter` reads it.
#[test]
fn paths_and_queries_are_read_decoded() {
  assert_eq!(
    routed("GET", "/my%20repo/sub%2Bdir/info/refs?service=git%2Dupload-pack", &[]),
    Ok(Route::Advertise {
      repo: "my repo/sub+dir".to_owned(),
      service: Service::UploadPack,
      version: Version::V0,
      head_only: false,
    })
  );
  assert_eq!(
    routed("GET", "/r/info/ref%73?service=git-frobnicate&service=git-receive-pack", &[]),
    Ok(Route::Advertise {
      repo: "r".to_owned(),
      service: Service::ReceivePack,
      version: Version::V0,
      head_only: false,
    })
  );
  assert_eq!(
    routed("GET", "/r/info/refs?service=git-upload-pack&service=git-frobnicate", &[]),
    Err(RouteRefused::UnsupportedService)
  );
  assert_eq!(
    routed("GET", "http://git.example/r/info/refs?service=git-upload-pack", &[]),
    Ok(Route::Advertise {
      repo: "r".to_owned(),
      service: Service::UploadPack,
      version: Version::V0,
      head_only: false,
    })
  );
}

/// A-595: do route a path under an admission that holds it and under one a byte short. Expect the decoded path charged
/// exactly, and the short one refused with nothing charged.
#[test]
fn a_route_charges_the_path_it_keeps() {
  let bytes = b"GET /team/repo.git/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: h\r\n\r\n";
  let head = head(bytes);
  let kept = to_u64("team/repo.git/info/refs".len());
  let mut admission = Admission::new(kept + 3);
  assert!(route(&head, &mut admission).is_ok());
  assert_eq!(admission.left(), 3);
  let mut admission = Admission::new(kept - 1);
  assert_eq!(route(&head, &mut admission), Err(RouteRefused::PastRoom));
  assert_eq!(admission.left(), kept - 1);
}

/// A-595: do write each service's prelude at each version. Expect gitprotocol-http's `# service=` pkt-line and a flush
/// for v0 and v1, byte for byte, and nothing for v2.
#[test]
fn the_prelude_names_the_service_for_v0_and_v1_alone() {
  let cases: [(Service, Version, &[u8]); 6] = [
    (Service::UploadPack, Version::V0, b"001e# service=git-upload-pack\n0000"),
    (Service::UploadPack, Version::V1, b"001e# service=git-upload-pack\n0000"),
    (Service::UploadPack, Version::V2, b""),
    (Service::ReceivePack, Version::V0, b"001f# service=git-receive-pack\n0000"),
    (Service::ReceivePack, Version::V1, b"001f# service=git-receive-pack\n0000"),
    (Service::ReceivePack, Version::V2, b""),
  ];
  for (service, version, expected) in cases {
    let mut out = Vec::new();
    advertisement_prelude(service, version, &mut out).unwrap();
    assert_eq!(out, expected, "{service:?} {version:?}");
  }
}

/// A-595: do write each smart reply's head over each version. Expect `200`, `http-backend`'s three cache fields and
/// then the content type, in its order, chunked to HTTP/1.1 and framed by the close to HTTP/1.0, byte for byte.
#[test]
fn a_smart_reply_head_is_written_as_http_backend_answers() {
  let mut out = Vec::new();
  reply_head(Reply::Advertisement(Service::UploadPack), HttpVersion::Http11, true, &mut out).unwrap();
  assert_eq!(
    out,
    b"HTTP/1.1 200 OK\r\nExpires: Fri, 01 Jan 1980 00:00:00 GMT\r\nPragma: no-cache\r\nCache-Control: no-cache, max-age=0, must-revalidate\r\nContent-Type: application/x-git-upload-pack-advertisement\r\nTransfer-Encoding: chunked\r\n\r\n"
  );
  let mut out = Vec::new();
  reply_head(Reply::Result(Service::ReceivePack), HttpVersion::Http11, false, &mut out).unwrap();
  assert_eq!(
    out,
    b"HTTP/1.1 200 OK\r\nExpires: Fri, 01 Jan 1980 00:00:00 GMT\r\nPragma: no-cache\r\nCache-Control: no-cache, max-age=0, must-revalidate\r\nContent-Type: application/x-git-receive-pack-result\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
  );
  let mut out = Vec::new();
  reply_head(Reply::Result(Service::UploadPack), HttpVersion::Http10, true, &mut out).unwrap();
  assert_eq!(
    out,
    b"HTTP/1.1 200 OK\r\nExpires: Fri, 01 Jan 1980 00:00:00 GMT\r\nPragma: no-cache\r\nCache-Control: no-cache, max-age=0, must-revalidate\r\nContent-Type: application/x-git-upload-pack-result\r\nConnection: close\r\n\r\n"
  );
}

/// A-595: do write the replies to a `405` over each version and a `403`. Expect each status, `Allow` with the route's
/// methods on a `405` alone and before the cache fields, as `bad_request` writes them, no body, the connection closed.
#[test]
fn a_refusal_reply_is_written_as_http_backend_answers() {
  let cache = "Expires: Fri, 01 Jan 1980 00:00:00 GMT\r\nPragma: no-cache\r\nCache-Control: no-cache, max-age=0, must-revalidate\r\n";
  let cases: [(RouteRefused, HttpVersion, String); 4] = [
    (
      RouteRefused::MethodNotAllowed(Allowed::GetHead),
      HttpVersion::Http11,
      format!("HTTP/1.1 405 Method Not Allowed\r\nAllow: GET, HEAD\r\n{cache}Content-Length: 0\r\nConnection: close\r\n\r\n"),
    ),
    (
      RouteRefused::MethodNotAllowed(Allowed::Post),
      HttpVersion::Http11,
      format!("HTTP/1.1 405 Method Not Allowed\r\nAllow: POST\r\n{cache}Content-Length: 0\r\nConnection: close\r\n\r\n"),
    ),
    (
      RouteRefused::MethodNotAllowed(Allowed::Post),
      HttpVersion::Http10,
      format!("HTTP/1.1 400 Bad Request\r\n{cache}Content-Length: 0\r\nConnection: close\r\n\r\n"),
    ),
    (
      RouteRefused::DumbProtocol,
      HttpVersion::Http11,
      format!("HTTP/1.1 403 Forbidden\r\n{cache}Content-Length: 0\r\nConnection: close\r\n\r\n"),
    ),
  ];
  for (refusal, http, expected) in cases {
    let mut out = Vec::new();
    refusal_reply(refusal, http, &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), expected, "{refusal:?} {http:?}");
  }
}
