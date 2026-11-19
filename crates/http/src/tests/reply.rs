use super::{BadField, Body, CONTINUE, LAST_CHUNK, Status, chunk, head, refusal};

/// A-595: do write a chunked reply's head kept open, and one closed. Expect the status line, the fields in order, the
/// framing, `Connection: close` only when asked, and the empty line, byte for byte.
#[test]
fn a_chunked_head_is_written_byte_for_byte() {
  let fields: [(&[u8], &[u8]); 2] = [
    (b"Content-Type", b"application/x-git-upload-pack-result"),
    (b"Cache-Control", b"no-cache"),
  ];
  let mut out = Vec::new();
  head(Status::OK, &fields, Body::Chunked, false, &mut out).unwrap();
  assert_eq!(
    out,
    b"HTTP/1.1 200 OK\r\nContent-Type: application/x-git-upload-pack-result\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n\r\n"
  );
  let mut out = Vec::new();
  head(Status::OK, &fields[..1], Body::Chunked, true, &mut out).unwrap();
  assert_eq!(
    out,
    b"HTTP/1.1 200 OK\r\nContent-Type: application/x-git-upload-pack-result\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
  );
}

/// A-595: do write a head framed by length and one framed by the close, and a refusal. Expect `Content-Length` with its
/// value; no framing field and the connection closed though not asked; a refusal empty and closed.
#[test]
fn each_framing_is_written_and_a_refusal_closes() {
  let mut out = Vec::new();
  head(Status::NOT_FOUND, &[], Body::Length(12), false, &mut out).unwrap();
  assert_eq!(out, b"HTTP/1.1 404 Not Found\r\nContent-Length: 12\r\n\r\n");
  let mut out = Vec::new();
  head(Status::OK, &[], Body::UntilClose, false, &mut out).unwrap();
  assert_eq!(out, b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
  let mut out = Vec::new();
  refusal(
    Status::METHOD_NOT_ALLOWED,
    &[(b"Allow", b"GET, HEAD")],
    &mut out,
  )
  .unwrap();
  assert_eq!(
    out,
    b"HTTP/1.1 405 Method Not Allowed\r\nAllow: GET, HEAD\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
  );
}

/// A-595: do write heads whose fields could split the reply: a CR, an LF or a NUL in a value, a name with a space or a
/// colon, an empty name. Expect each refused with nothing written, and HTAB in a value written.
#[test]
fn a_field_that_could_split_the_reply_is_refused_with_nothing_written() {
  let cases: [(&[u8], &[u8]); 6] = [
    (b"Location", b"/a\r\nSet-Cookie: x"),
    (b"Location", b"/a\nb"),
    (b"Location", b"/a\0b"),
    (b"Bad Name", b"x"),
    (b"Bad:Name", b"x"),
    (b"", b"x"),
  ];
  for field in cases {
    let mut out = b"kept".to_vec();
    assert_eq!(
      head(Status::OK, &[field], Body::Length(0), true, &mut out),
      Err(BadField),
      "{field:?}"
    );
    assert_eq!(out, b"kept");
  }
  let mut out = Vec::new();
  head(
    Status::OK,
    &[(b"X", b"a\tb")],
    Body::Length(0),
    false,
    &mut out,
  )
  .unwrap();
  assert_eq!(
    out,
    b"HTTP/1.1 200 OK\r\nX: a\tb\r\nContent-Length: 0\r\n\r\n"
  );
}

/// A-595: do write chunks of several sizes, an empty one, the last chunk and the interim `100 Continue`. Expect each
/// size in lowercase hexadecimal, nothing for no data, and RFC 9112's own framing.
#[test]
fn chunks_are_framed_by_their_hexadecimal_size() {
  let mut out = Vec::new();
  chunk(b"0123456789", &mut out);
  chunk(b"", &mut out);
  chunk(&[b'x'; 300], &mut out);
  let mut expected = b"a\r\n0123456789\r\n12c\r\n".to_vec();
  expected.extend_from_slice(&[b'x'; 300]);
  expected.extend_from_slice(b"\r\n");
  assert_eq!(out, expected);
  assert_eq!(LAST_CHUNK, b"0\r\n\r\n");
  assert_eq!(CONTINUE, b"HTTP/1.1 100 Continue\r\n\r\n");
}

/// A-595: do read every status's code and reason. Expect RFC 9110's, and RFC 6585's for 431.
#[test]
fn each_status_is_rfc_9110s() {
  let cases: [(Status, u16, &str); 17] = [
    (Status::CONTINUE, 100, "Continue"),
    (Status::OK, 200, "OK"),
    (Status::BAD_REQUEST, 400, "Bad Request"),
    (Status::UNAUTHORIZED, 401, "Unauthorized"),
    (Status::FORBIDDEN, 403, "Forbidden"),
    (Status::NOT_FOUND, 404, "Not Found"),
    (Status::METHOD_NOT_ALLOWED, 405, "Method Not Allowed"),
    (Status::LENGTH_REQUIRED, 411, "Length Required"),
    (Status::CONTENT_TOO_LARGE, 413, "Content Too Large"),
    (Status::URI_TOO_LONG, 414, "URI Too Long"),
    (
      Status::UNSUPPORTED_MEDIA_TYPE,
      415,
      "Unsupported Media Type",
    ),
    (Status::EXPECTATION_FAILED, 417, "Expectation Failed"),
    (
      Status::FIELDS_TOO_LARGE,
      431,
      "Request Header Fields Too Large",
    ),
    (Status::INTERNAL_SERVER_ERROR, 500, "Internal Server Error"),
    (Status::NOT_IMPLEMENTED, 501, "Not Implemented"),
    (Status::SERVICE_UNAVAILABLE, 503, "Service Unavailable"),
    (
      Status::VERSION_NOT_SUPPORTED,
      505,
      "HTTP Version Not Supported",
    ),
  ];
  for (status, code, reason) in cases {
    assert_eq!((status.code, status.reason), (code, reason));
  }
}
