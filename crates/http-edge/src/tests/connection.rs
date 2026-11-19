use std::io::{Read as _, Write as _};

use flate2::Compression;
use flate2::write::GzEncoder;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore};
use slates_http::head::Head;
use slates_http::reply::Status;

use super::{Connection, Decision, Ended, Fields, Next, Phase};
use crate::credential::{Credential, credential};
use crate::tls::{ALPN_HTTP_1_1, HttpsSession, ServingIdentity};

/// Room enough for every request these tests send but the bomb's.
const ROOM: u64 = 1 << 24;

/// What a driver saw of a connection.
#[derive(Debug, Default, PartialEq, Eq)]
struct Seen {
  /// Every byte the connection wrote.
  written: Vec<u8>,
  /// Each request's body as the service took it, in order.
  bodies: Vec<Vec<u8>>,
  /// How many heads it decided.
  decided: usize,
  /// Whether it closed its write side for a staged close.
  staged: bool,
  /// How it finished: drained after a refusal, or closed.
  drained: bool,
  /// Whether it closed.
  closed: bool,
}

/// A site's decision on every head, and the service's reply to every body.
struct Script<'s> {
  /// The decision.
  decision: &'s dyn Fn(&Head<'_>) -> Decision,
  /// The reply's runs.
  reply: &'s [&'s [u8]],
}

/// Drives `connection` as its listener would, `pieces` the socket's reads in order (then its end), `script` the site
/// and the service.
fn drive(connection: &mut Connection, pieces: &[&[u8]], script: &Script<'_>) -> Seen {
  let mut seen = Seen::default();
  let mut pieces = pieces.iter();
  let (mut body, mut replies) = (Vec::new(), script.reply.iter());
  loop {
    match connection.next() {
      Next::Write => seen.written.extend(connection.take_output()),
      Next::Read => match pieces.next() {
        Some(piece) => connection.receive(piece),
        None => connection.peer_closed(),
      },
      Next::Decide => {
        let decision = (script.decision)(&connection.head().unwrap());
        seen.decided += 1;
        connection.decide(decision);
      }
      Next::Body(run) => {
        body.extend_from_slice(run);
        connection.body_taken();
      }
      Next::BodyEnd => {
        seen.bodies.push(std::mem::take(&mut body));
        replies = script.reply.iter();
      }
      Next::Reply => match replies.next() {
        Some(run) => connection.reply(run),
        None => connection.reply_end(),
      },
      Next::CloseWrite => seen.staged = true,
      Next::Drain => {
        seen.drained = true;
        return seen;
      }
      Next::Close => {
        seen.closed = true;
        return seen;
      }
    }
  }
}

/// Fields of a reply: `name: value` pairs.
fn fields(pairs: &[(&str, &str)]) -> Fields {
  pairs
    .iter()
    .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
    .collect()
}

/// The site that serves every request with an advertisement's content type, the head alone to `HEAD`.
fn serve(head: &Head<'_>) -> Decision {
  Decision::Serve {
    fields: fields(&[(
      "Content-Type",
      "application/x-git-upload-pack-advertisement",
    )]),
    head_only: head.method == b"HEAD",
  }
}

/// The site that refuses every request `401` with a bearer challenge.
fn unauthorized(_: &Head<'_>) -> Decision {
  Decision::Refuse {
    status: Status::UNAUTHORIZED,
    fields: fields(&[("WWW-Authenticate", "Bearer realm=\"slates\"")]),
  }
}

/// A served reply's head with `extra` before its empty line.
fn served_head(extra: &str) -> String {
  format!(
    "HTTP/1.1 200 OK\r\nContent-Type: application/x-git-upload-pack-advertisement\r\nTransfer-Encoding: chunked\r\n{extra}\r\n"
  )
}

/// The advertisement's two runs, as a reply carries them.
const RUNS: [&[u8]; 2] = [b"001e# service=git-upload-pack\n", b"0000"];

/// [`RUNS`] chunked, and the last chunk.
fn chunked_runs() -> String {
  "1e\r\n001e# service=git-upload-pack\n\r\n4\r\n0000\r\n0\r\n\r\n".to_owned()
}

/// A GET for the advertisement, HTTP/1.1.
const GET: &[u8] = b"GET /r.git/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: h\r\n\r\n";

/// A-625: do send a GET and end the stream after the reply. Expect one decision, the service's runs chunked after the
/// decision's head, the connection kept for a next request, and its end between requests the client's ordinary close.
#[test]
fn a_get_is_decided_answered_in_chunks_and_kept() {
  let mut connection = Connection::clear(ROOM);
  let script = Script {
    decision: &serve,
    reply: &RUNS,
  };
  let seen = drive(&mut connection, &[GET], &script);
  let expected = format!("{}{}", served_head(""), chunked_runs());
  assert_eq!(String::from_utf8(seen.written).unwrap(), expected);
  assert_eq!(
    (seen.decided, seen.bodies, seen.closed),
    (1, vec![Vec::new()], true)
  );
  assert_eq!(connection.ended(), Some(Ended::Peer));
}

/// A POST of `body` with `framing_fields` (each a line), HTTP/1.1.
fn post(framing_fields: &[&str], body: &[u8]) -> Vec<u8> {
  let mut out = b"POST /r.git/git-upload-pack HTTP/1.1\r\nHost: h\r\n".to_vec();
  for field in framing_fields {
    out.extend_from_slice(field.as_bytes());
    out.extend_from_slice(b"\r\n");
  }
  out.extend_from_slice(b"\r\n");
  out.extend_from_slice(body);
  out
}

/// A-625: do POST a `Content-Length` body, the request cut in two at every place. Expect the service to take the whole
/// body each time, and the same reply.
#[test]
fn a_body_reaches_the_service_whole_whatever_its_cuts() {
  let request = post(&["Content-Length: 10"], b"0123456789");
  let script = Script {
    decision: &serve,
    reply: &RUNS,
  };
  for cut in 0..=request.len() {
    let mut connection = Connection::clear(ROOM);
    let seen = drive(
      &mut connection,
      &[&request[..cut], &request[cut..]],
      &script,
    );
    assert_eq!(seen.bodies, vec![b"0123456789".to_vec()], "cut at {cut}");
    assert_eq!(connection.ended(), Some(Ended::Peer), "cut at {cut}");
  }
}

/// `data` gzipped.
fn gzipped(data: &[u8]) -> Vec<u8> {
  let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
  encoder.write_all(data).unwrap();
  encoder.finish().unwrap()
}

/// `data` chunked in runs of `size`, then the last chunk.
fn chunked(data: &[u8], size: usize) -> Vec<u8> {
  let mut out = Vec::new();
  for run in data.chunks(size) {
    out.extend_from_slice(format!("{:x}\r\n", run.len()).as_bytes());
    out.extend_from_slice(run);
    out.extend_from_slice(b"\r\n");
  }
  out.extend_from_slice(b"0\r\n\r\n");
  out
}

/// A-625: do POST a gzip body chunked, read in seven-byte pieces. Expect the service to take the bytes git gzipped,
/// inflated, whole.
#[test]
fn a_chunked_gzip_body_reaches_the_service_inflated() {
  let text: Vec<u8> = (0..200)
    .flat_map(|line| format!("0032have {line:040x}\n").into_bytes())
    .collect();
  let request = post(
    &["Transfer-Encoding: chunked", "Content-Encoding: gzip"],
    &chunked(&gzipped(&text), 100),
  );
  let pieces: Vec<&[u8]> = request.chunks(7).collect();
  let mut connection = Connection::clear(ROOM);
  let seen = drive(
    &mut connection,
    &pieces,
    &Script {
      decision: &serve,
      reply: &RUNS,
    },
  );
  assert_eq!(seen.bodies, vec![text]);
  assert_eq!(connection.ended(), Some(Ended::Peer));
}

/// A-625: do POST with `Expect: 100-continue`, the body sent only after the head, and again with the body in the same
/// read as the head. Expect `100 Continue` before the reply when the client waits for it, and none when its body came.
#[test]
fn a_waiting_client_is_told_to_continue() {
  let head = post(&["Content-Length: 4", "Expect: 100-continue"], b"");
  let script = Script {
    decision: &serve,
    reply: &RUNS,
  };
  let mut connection = Connection::clear(ROOM);
  let seen = drive(&mut connection, &[&head, b"abcd"], &script);
  assert!(
    seen
      .written
      .starts_with(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\n")
  );
  assert_eq!(seen.bodies, vec![b"abcd".to_vec()]);
  let whole = post(&["Content-Length: 4", "Expect: 100-continue"], b"abcd");
  let mut connection = Connection::clear(ROOM);
  let seen = drive(&mut connection, &[&whole], &script);
  assert!(seen.written.starts_with(b"HTTP/1.1 200 OK\r\n"));
}

/// A-625: do send two GETs in one read. Expect both decided and answered in turn on the one connection.
#[test]
fn pipelined_requests_are_served_in_turn() {
  let both = [GET, GET].concat();
  let mut connection = Connection::clear(ROOM);
  let seen = drive(
    &mut connection,
    &[&both],
    &Script {
      decision: &serve,
      reply: &RUNS,
    },
  );
  let one = format!("{}{}", served_head(""), chunked_runs());
  assert_eq!(String::from_utf8(seen.written).unwrap(), one.repeat(2));
  assert_eq!(seen.decided, 2);
}

/// A-625: do send a HEAD for the advertisement. Expect the head a GET would get, and no body: no chunk, no last chunk.
#[test]
fn a_head_request_gets_the_head_alone() {
  let head = b"HEAD /r.git/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: h\r\n\r\n";
  let mut connection = Connection::clear(ROOM);
  let seen = drive(
    &mut connection,
    &[head],
    &Script {
      decision: &serve,
      reply: &[],
    },
  );
  assert_eq!(String::from_utf8(seen.written).unwrap(), served_head(""));
}

/// A-625: do send an HTTP/1.0 GET. Expect the reply framed by the connection's close (no chunks), and the connection
/// closed by the reply.
#[test]
fn http_1_0_is_answered_until_the_close() {
  let get = b"GET /r.git/info/refs?service=git-upload-pack HTTP/1.0\r\n\r\n";
  let mut connection = Connection::clear(ROOM);
  let seen = drive(
    &mut connection,
    &[get],
    &Script {
      decision: &serve,
      reply: &RUNS,
    },
  );
  let expected = "HTTP/1.1 200 OK\r\nContent-Type: application/x-git-upload-pack-advertisement\r\nConnection: close\r\n\r\n001e# service=git-upload-pack\n0000";
  assert_eq!(String::from_utf8(seen.written).unwrap(), expected);
  assert_eq!((seen.closed, seen.staged), (true, false));
  assert_eq!(connection.ended(), Some(Ended::Completed));
}

/// A-625: do refuse a GET `401`, the client's next request in the same read. Expect the refusal with its challenge and
/// no close, then the next request decided on the same connection.
#[test]
fn a_refusal_of_a_bodiless_request_keeps_the_connection() {
  let both = [GET, GET].concat();
  let mut connection = Connection::clear(ROOM);
  let seen = drive(
    &mut connection,
    &[&both],
    &Script {
      decision: &unauthorized,
      reply: &[],
    },
  );
  let one = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"slates\"\r\nContent-Length: 0\r\n\r\n";
  assert_eq!(String::from_utf8(seen.written).unwrap(), one.repeat(2));
  assert_eq!((seen.decided, seen.staged), (2, false));
}

/// A-625: do refuse a POST whose body has not been read. Expect the refusal with `Connection: close`, the write side
/// closed and the rest drained: the staged close.
#[test]
fn a_refusal_with_its_body_unread_closes_in_stages() {
  let request = post(&["Content-Length: 5"], b"hello");
  let mut connection = Connection::clear(ROOM);
  let seen = drive(
    &mut connection,
    &[&request],
    &Script {
      decision: &unauthorized,
      reply: &[],
    },
  );
  let expected = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"slates\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
  assert_eq!(String::from_utf8(seen.written).unwrap(), expected);
  assert_eq!(
    (seen.staged, seen.drained, seen.bodies.len()),
    (true, true, 0)
  );
  assert_eq!(
    connection.ended(),
    Some(Ended::Refused(Status::UNAUTHORIZED))
  );
}

/// A-625: do send a head with whitespace before a field's colon. Expect it refused `400` before any decision, closed in
/// stages.
#[test]
fn a_malformed_head_is_refused_and_closed_in_stages() {
  let mut connection = Connection::clear(ROOM);
  let request = b"GET / HTTP/1.1\r\nHost : h\r\n\r\n";
  let seen = drive(
    &mut connection,
    &[request],
    &Script {
      decision: &serve,
      reply: &[],
    },
  );
  let expected = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
  assert_eq!(String::from_utf8(seen.written).unwrap(), expected);
  assert_eq!((seen.decided, seen.staged, seen.drained), (0, true, true));
  assert_eq!(
    connection.ended(),
    Some(Ended::Refused(Status::BAD_REQUEST))
  );
}

/// A-625: do POST a chunked body whose chunk is not followed by its CRLF. Expect the service to have taken the bytes
/// before the break and no end, and the request refused `400` in stages.
#[test]
fn a_broken_chunked_body_is_refused_mid_stream() {
  let request = post(&["Transfer-Encoding: chunked"], b"5\r\nhelloX\r\n0\r\n\r\n");
  let mut connection = Connection::clear(ROOM);
  let seen = drive(
    &mut connection,
    &[&request],
    &Script {
      decision: &serve,
      reply: &RUNS,
    },
  );
  assert!(seen.written.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
  assert_eq!(
    (seen.bodies.len(), seen.staged, seen.drained),
    (0, true, true)
  );
}

/// A-625: do POST sixteen mebibytes of zeros gzipped, under a request room of one mebibyte. Expect it refused `413` in
/// stages, the service never past the room.
#[test]
fn a_gzip_bomb_is_refused_at_the_room() {
  let bomb = gzipped(&vec![0u8; 16 << 20]);
  let request = post(
    &[
      &format!("Content-Length: {}", bomb.len()),
      "Content-Encoding: gzip",
    ],
    &bomb,
  );
  let pieces: Vec<&[u8]> = request.chunks(4096).collect();
  let mut connection = Connection::clear(1 << 20);
  let seen = drive(
    &mut connection,
    &pieces,
    &Script {
      decision: &serve,
      reply: &RUNS,
    },
  );
  assert!(
    seen
      .written
      .starts_with(b"HTTP/1.1 413 Content Too Large\r\n")
  );
  assert_eq!((seen.staged, seen.drained), (true, true));
}

/// A-625: do end the stream in the middle of a head, and before any byte. Expect the first ended as truncated and the
/// second as the client's ordinary close, neither answered.
#[test]
fn a_client_that_leaves_ends_the_connection() {
  let mut connection = Connection::clear(ROOM);
  let seen = drive(
    &mut connection,
    &[&GET[..10]],
    &Script {
      decision: &serve,
      reply: &[],
    },
  );
  assert_eq!(
    (seen.written.len(), connection.ended()),
    (0, Some(Ended::Truncated))
  );
  let mut connection = Connection::clear(ROOM);
  let seen = drive(
    &mut connection,
    &[],
    &Script {
      decision: &serve,
      reply: &[],
    },
  );
  assert_eq!(
    (seen.written.len(), connection.ended()),
    (0, Some(Ended::Peer))
  );
}

/// Drives a GET to its reply and calls `fail` with `status` after `runs` of the reply.
fn fail_after(runs: usize, status: Status) -> (Vec<u8>, Option<Ended>) {
  let mut connection = Connection::clear(ROOM);
  connection.receive(GET);
  let mut written = Vec::new();
  let mut sent = 0;
  loop {
    match connection.next() {
      Next::Write => written.extend(connection.take_output()),
      Next::Decide => connection.decide(serve(&connection.head().unwrap())),
      Next::Reply if sent == runs => connection.fail(status),
      Next::Reply => {
        connection.reply(RUNS[sent]);
        sent += 1;
      }
      Next::Read | Next::Body(_) | Next::BodyEnd | Next::CloseWrite => {}
      Next::Drain | Next::Close => return (written, connection.ended()),
    }
  }
}

/// A-625: do fail the service before its reply and after its first run. Expect a `500` refusal in the first case, and
/// in the second the reply stopped short of its last chunk, the connection aborted.
#[test]
fn a_failing_service_is_refused_before_its_reply_and_aborted_after() {
  let (written, ended) = fail_after(0, Status::INTERNAL_SERVER_ERROR);
  assert!(written.starts_with(b"HTTP/1.1 500 Internal Server Error\r\n"));
  assert_eq!(ended, Some(Ended::Refused(Status::INTERNAL_SERVER_ERROR)));
  let (written, ended) = fail_after(1, Status::INTERNAL_SERVER_ERROR);
  let expected = format!(
    "{}1e\r\n001e# service=git-upload-pack\n\r\n",
    served_head("")
  );
  assert_eq!(String::from_utf8(written).unwrap(), expected);
  assert_eq!(ended, Some(Ended::Aborted));
}

/// What `connection` needs next, as its debug text, and its phase once it has said so.
fn step(connection: &mut Connection) -> (String, Phase) {
  let next = format!("{:?}", connection.next());
  (next, connection.phase())
}

/// A-625: do step a POST through its phases. Expect idle before a byte, head while it comes and waits for a decision,
/// body while its body comes, reply from its end while the service answers, idle again after.
#[test]
fn the_phase_follows_the_exchange() {
  let request = post(&["Content-Length: 4"], b"");
  let mut connection = Connection::clear(ROOM);
  let mut seen = vec![step(&mut connection)];
  connection.receive(&request[..5]);
  seen.push(step(&mut connection));
  connection.receive(&request[5..]);
  seen.push(step(&mut connection));
  connection.decide(serve(&connection.head().unwrap()));
  seen.push(step(&mut connection));
  connection.receive(b"abcd");
  seen.push(step(&mut connection));
  connection.body_taken();
  seen.push(step(&mut connection));
  seen.push(step(&mut connection));
  connection.reply_end();
  seen.push(step(&mut connection));
  connection.take_output();
  seen.push(step(&mut connection));
  let expected = [
    ("Read", Phase::Idle),
    ("Read", Phase::Head),
    ("Decide", Phase::Head),
    ("Read", Phase::Body),
    ("Body([97, 98, 99, 100])", Phase::Body),
    ("BodyEnd", Phase::Reply),
    ("Reply", Phase::Reply),
    ("Write", Phase::Idle),
    ("Read", Phase::Idle),
  ]
  .map(|(next, phase)| (next.to_owned(), phase));
  assert_eq!(seen, expected);
}

/// A self-signed identity for `localhost`, and a client trusting it offering `http/1.1`.
fn tls_pair() -> (ServingIdentity, ClientConnection) {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  let private = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
  let identity = ServingIdentity::new(vec![cert.der().clone()], private).unwrap();
  let mut roots = RootCertStore::empty();
  roots.add(cert.der().clone()).unwrap();
  let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
  let mut config = ClientConfig::builder_with_provider(provider)
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
  config.alpn_protocols = vec![ALPN_HTTP_1_1.to_vec()];
  let name = ServerName::try_from("localhost").unwrap();
  let client = ClientConnection::new(std::sync::Arc::new(config), name).unwrap();
  (identity, client)
}

/// Drives `connection` against `client` until the client has read a reply's last chunk: the client's request is
/// `request`, the site decides with `decide` and the service answers [`RUNS`]. Returns the plaintext the client read and
/// the credential the decision read.
fn drive_tls(
  connection: &mut Connection,
  client: &mut ClientConnection,
  request: &[u8],
) -> (Vec<u8>, Option<Credential>) {
  client.writer().write_all(request).unwrap();
  let (mut read, mut seen_credential, mut replies) = (Vec::new(), None, RUNS.iter());
  while !read.ends_with(b"0\r\n\r\n") {
    match connection.next() {
      Next::Write => {
        let output = connection.take_output();
        let mut rest = output.as_slice();
        while !rest.is_empty() {
          client.read_tls(&mut rest).unwrap();
          client.process_new_packets().unwrap();
          let mut chunk = [0u8; 4096];
          while let Ok(count @ 1..) = client.reader().read(&mut chunk) {
            read.extend_from_slice(&chunk[..count]);
          }
        }
      }
      Next::Read => {
        let mut bytes = Vec::new();
        while client.wants_write() {
          client.write_tls(&mut bytes).unwrap();
        }
        connection.receive(&bytes);
      }
      Next::Decide => {
        let head = connection.head().unwrap();
        seen_credential = credential(&head, connection.secure()).unwrap();
        let decision = serve(&head);
        connection.decide(decision);
      }
      Next::Reply => match replies.next() {
        Some(run) => connection.reply(run),
        None => connection.reply_end(),
      },
      Next::BodyEnd => {}
      other => panic!("{other:?}"),
    }
  }
  (read, seen_credential)
}

/// A-625: do send a GET with Basic credentials over TLS from a rustls client. Expect the handshake, the credentials read
/// on a secure connection, and the chunked reply read by the client in plaintext.
#[test]
fn a_get_over_tls_is_served_and_its_basic_credentials_read() {
  let (identity, mut client) = tls_pair();
  let session = HttpsSession::new(identity.connection().unwrap());
  let mut connection = Connection::tls(session, ROOM);
  assert_eq!(
    (connection.secure(), connection.phase()),
    (true, Phase::Handshake)
  );
  let request = b"GET /r.git/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==\r\n\r\n";
  let (read, seen) = drive_tls(&mut connection, &mut client, request);
  assert_eq!(
    String::from_utf8(read).unwrap(),
    format!("{}{}", served_head(""), chunked_runs())
  );
  let basic = Credential::Basic {
    user: b"Aladdin".to_vec(),
    password: b"open sesame".to_vec(),
  };
  assert_eq!(seen, Some(basic));
}
