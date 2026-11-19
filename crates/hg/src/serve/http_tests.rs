//! Tests of HTTP version 1 serving (A-650 part 1). The golden: a stock 7.2 client's clone, pull and incoming over HTTP
//! against stock `hg serve` 7.2 (`fixtures/serve-http`, the repository of `fixtures/serve` made the same way, its
//! states equal byte for byte), each request served again from the store the SSH sessions' tests rebuild, and each
//! answer compared by meaning: strings byte for byte in the media type 7.2 named, the capabilities a subset of 7.2's,
//! and each `getbundle` inflated from its `0.2` engine and compared as the session's golden compares a stream. Then
//! 7.2's own words for what it refuses, the abort and the zlib framings, a chooser that breaks its contract, requests
//! 7.2 would misread, and arbitrary input.

use super::super::getbundle::tests::{
  Served, check_bundle2_subset, check_streams, cloned, session_stores,
};
use super::super::session::Refused;
use super::{Answered, Compression, Compressor, Media, Permission, answer, command, permission};
use crate::compression::{self, EncodeRefusal, Engine};
use crate::http::{self, MEDIA_TYPE_ERROR, MEDIA_TYPE_V1, MEDIA_TYPE_V2, Request};
use crate::{Admission, PastAdmission, capabilities};
use proptest::prelude::*;

/// Format: `httpheader`'s value in the golden, 7.2's default (`server.maxhttpheaderlen`, 1024), so the capability
/// reads as 7.2 wrote it.
const HEADER_LINE: u64 = 1024;

/// Format: a request's room, as the session's tests admit one.
const ROOM: u64 = 1 << 20;

/// One captured exchange: what the client sent, and the head and body (de-chunked) 7.2 answered.
#[derive(Clone, Copy)]
struct Exchange {
  request: &'static [u8],
  head: &'static [u8],
  body: &'static [u8],
}

/// An exchange of `fixtures/serve-http` by its session and number.
macro_rules! exchange {
  ($session:literal, $number:literal) => {
    Exchange {
      request: include_bytes!(concat!(
        "../../fixtures/serve-http/",
        $session,
        ".",
        $number,
        ".request"
      )),
      head: include_bytes!(concat!(
        "../../fixtures/serve-http/",
        $session,
        ".",
        $number,
        ".response-head"
      )),
      body: include_bytes!(concat!(
        "../../fixtures/serve-http/",
        $session,
        ".",
        $number,
        ".response-body"
      )),
    }
  };
}

/// The captured sessions' exchanges, in the order the script ran them: the clone, the pull, the incoming.
fn sessions() -> [Vec<Exchange>; 3] {
  [
    vec![
      exchange!("http-clone", "01"),
      exchange!("http-clone", "02"),
      exchange!("http-clone", "03"),
    ],
    vec![
      exchange!("http-pull", "01"),
      exchange!("http-pull", "02"),
      exchange!("http-pull", "03"),
    ],
    vec![
      exchange!("http-incoming", "01"),
      exchange!("http-incoming", "02"),
      exchange!("http-incoming", "03"),
      exchange!("http-incoming", "04"),
    ],
  ]
}

/// A captured request's parts as an HTTP server reads them: the query after the target's `?`, every field (its value
/// without surrounding whitespace), and the body after the head.
struct Parsed {
  query: Vec<u8>,
  fields: Vec<(Vec<u8>, Vec<u8>)>,
  body: Vec<u8>,
}

impl Parsed {
  fn of(raw: &[u8]) -> Parsed {
    let end = raw.windows(4).position(|four| four == b"\r\n\r\n").unwrap();
    let mut lines = raw[..end]
      .split(|byte| *byte == b'\n')
      .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    let target = lines
      .next()
      .unwrap()
      .split(|byte| *byte == b' ')
      .nth(1)
      .unwrap();
    let query = target
      .splitn(2, |byte| *byte == b'?')
      .nth(1)
      .unwrap_or_default()
      .to_vec();
    let fields = lines
      .map(|line| {
        let colon = line.iter().position(|byte| *byte == b':').unwrap();
        (
          line[..colon].to_vec(),
          line[colon + 1..].trim_ascii().to_vec(),
        )
      })
      .collect();
    Parsed {
      query,
      fields,
      body: raw[end + 4..].to_vec(),
    }
  }

  /// The command the query names.
  fn command(&self) -> Vec<u8> {
    let start = self
      .query
      .windows(4)
      .position(|four| four == b"cmd=")
      .unwrap()
      + 4;
    self.query[start..]
      .split(|byte| *byte == b'&')
      .next()
      .unwrap()
      .to_vec()
  }
}

/// What 7.2's head names as the body's media type.
fn content_type(head: &[u8]) -> Vec<u8> {
  let line = head
    .split(|byte| *byte == b'\n')
    .find(|line| line.to_ascii_lowercase().starts_with(b"content-type:"))
    .unwrap();
  line[b"content-type:".len()..].trim_ascii().to_vec()
}

/// A chooser that answers the client's first engine at `level`, and counts its calls.
struct First {
  level: i32,
  calls: Vec<(Media, Vec<Engine>)>,
}

impl First {
  fn at(level: i32) -> First {
    First {
      level,
      calls: Vec::new(),
    }
  }
}

impl Compressor for First {
  fn choose(&mut self, media: Media, engines: &[Engine], _length: u64) -> Compression {
    self.calls.push((media, engines.to_vec()));
    Compression {
      engine: engines[0],
      level: self.level,
    }
  }
}

/// A chooser that answers what it is told, whatever the client reads.
struct Fixed(Compression);

impl Compressor for Fixed {
  fn choose(&mut self, _media: Media, _engines: &[Engine], _length: u64) -> Compression {
    self.0
  }
}

/// The answer `served` gives the captured `raw` request.
fn served_answer(served: &Served, raw: &[u8], compressor: &mut impl Compressor) -> Answered {
  let parsed = Parsed::of(raw);
  let fields: Vec<(&[u8], &[u8])> = parsed
    .fields
    .iter()
    .map(|(name, value)| (name.as_slice(), value.as_slice()))
    .collect();
  let request = Request {
    query: &parsed.query,
    headers: &fields,
    body: &parsed.body,
  };
  answer(
    served,
    &request,
    HEADER_LINE,
    compressor,
    &mut Admission::new(ROOM),
  )
}

/// The bundle2 stream an `application/mercurial-0.2` body carries, inflated from the engine its header names.
fn inflated(body: &[u8]) -> (Vec<u8>, Vec<u8>) {
  let (engine, payload) = http::decode_v2(body).unwrap();
  let mut stream = Vec::new();
  let engine_named = Engine::for_wire_name(engine).unwrap();
  compression::inflate(
    engine_named,
    payload,
    &mut Admission::new(ROOM),
    &mut stream,
  )
  .unwrap();
  (engine.to_vec(), stream)
}

/// Checks a `capabilities` answer: each capability one 7.2 advertised, with 7.2's value but `bundle2` (a subset of
/// 7.2's) and `compression` (engines this crate writes, 7.2's among them).
fn check_capabilities(ours: &[u8], theirs: &[u8]) {
  let ours = capabilities::decode(ours);
  let theirs = capabilities::decode(theirs);
  for (name, value) in ours.iter() {
    let held = theirs.get(name).unwrap();
    match name {
      b"bundle2" => check_bundle2_subset(value.unwrap(), held.unwrap()),
      b"compression" => {
        let ours: Vec<&[u8]> = value.unwrap().split(|byte| *byte == b',').collect();
        assert!(
          ours
            .iter()
            .all(|engine| Engine::for_wire_name(engine).is_some())
        );
        assert!(
          held
            .unwrap()
            .split(|byte| *byte == b',')
            .all(|engine| ours.contains(&engine))
        );
      }
      _ => assert_eq!(value, held, "{}", String::from_utf8_lossy(name)),
    }
  }
  let names: Vec<&[u8]> = ours.iter().map(|(name, _)| name).collect();
  let expected: [&[u8]; 8] = [
    b"batch",
    b"branchmap",
    b"bundle2",
    b"compression",
    b"getbundle",
    b"httpheader",
    b"httpmediatype",
    b"known",
  ];
  assert_eq!(names, expected);
}

/// Checks one served exchange against 7.2's answer to it (the module doc).
fn check_exchange(served: &Served, exchange: &Exchange, ours: &Answered) {
  let command = Parsed::of(exchange.request).command();
  let theirs_type = content_type(exchange.head);
  assert_eq!(
    (ours.media_type, ours.refused),
    (theirs_type.as_slice(), None)
  );
  match command.as_slice() {
    b"capabilities" => check_capabilities(&ours.body, exchange.body),
    b"getbundle" => {
      let (_, ours_stream) = inflated(&ours.body);
      let (engine, theirs_stream) = inflated(exchange.body);
      assert_eq!(engine, b"zstd");
      check_streams(served, &ours_stream, &theirs_stream);
    }
    _ => assert_eq!(
      String::from_utf8_lossy(&ours.body),
      String::from_utf8_lossy(exchange.body)
    ),
  }
}

/// AC-HG.1 (golden), A-650 part 1: do serve every exchange of a stock 7.2 client's clone, pull and incoming over HTTP
/// (each `capabilities`, `batch` of discovery, `getbundle` and the incoming's `listkeys` of the phases) from the store
/// holding what 7.2 had sent up to each session, the client's first engine chosen. Expect each answer in the media type
/// 7.2 named; `batch` and `listkeys` byte for byte; the capabilities a subset of 7.2's with 7.2's values; each
/// `getbundle` a `0.2` stream in zstd, as 7.2's, that inflates to what 7.2's meant; and the chooser asked once per
/// stream, with the engines the client listed that this crate writes, in its order.
#[test]
fn every_http_exchange_is_answered_as_mercurial_meant_it() {
  let mut answered = 0;
  let mut chooser = First::at(3);
  for ((served, _), exchanges) in session_stores().into_iter().zip(sessions()) {
    for exchange in &exchanges {
      let ours = served_answer(&served, exchange.request, &mut chooser);
      check_exchange(&served, exchange, &ours);
      answered += 1;
    }
  }
  let listed = vec![
    Engine::Zstd,
    Engine::Zlib,
    Engine::Uncompressed,
    Engine::Bzip2,
  ];
  assert_eq!(answered, 10);
  assert_eq!(chooser.calls, vec![(Media::V2, listed); 3]);
}

/// AC-HG.9 (hostile input), A-650 part 1: do ask for a command 7.2's table lacks, for none at all, and replay a bundle1
/// clone's `getbundle` (A-569's `http-oob` capture of stock 7.2). Expect the first two answered `application/hg-error`
/// in 7.2's words for a command not available over HTTP, and the bundle1 request refused in exactly the bytes 7.2
/// answered it with, each naming its refusal.
#[test]
fn requests_mercurial_refuses_are_refused_in_its_own_words() {
  let served = cloned();
  let not_available = &b"requested wire protocol command is not available over HTTP"[..];
  for query in [&b"cmd=frobnicate"[..], b"key=tip"] {
    let request = Request {
      query,
      headers: &[],
      body: b"",
    };
    let ours = answer(
      &served,
      &request,
      HEADER_LINE,
      &mut First::at(3),
      &mut Admission::new(ROOM),
    );
    assert_eq!(
      (ours.media_type, ours.body.as_slice(), ours.refused),
      (MEDIA_TYPE_ERROR, not_available, Some(Refused::NotAvailable))
    );
  }
  let oob = exchange!("../wire/http-oob", "04");
  let ours = served_answer(&served, oob.request, &mut First::at(3));
  assert_eq!(
    (ours.media_type, ours.body.as_slice()),
    (content_type(oob.head).as_slice(), oob.body)
  );
  assert!(matches!(
    ours.refused,
    Some(Refused::Serve(super::super::Refusal::NoBundle2))
  ));
}

/// A `getbundle` request of the cloned store from a client sending `proto` as what it reads, its bundle capabilities
/// `caps` (a bundle2 blob, unquoted).
fn getbundle_of(proto: Option<&[u8]>, blob: &[u8], chooser: &mut impl Compressor) -> Answered {
  let mut quoted = b"HG20,bundle2=".to_vec();
  capabilities::encode_bundle2_value(blob, &mut quoted);
  let mut arguments = Vec::new();
  let form: [(&[u8], &[u8]); 2] = [(b"bundlecaps", &quoted), (b"common", b"")];
  http::encode_form(&form, &mut arguments);
  let mut fields: Vec<(&[u8], &[u8])> = Vec::new();
  fields.push((b"X-HgArg-1", &arguments));
  if let Some(proto) = proto {
    fields.push((b"X-HgProto-1", proto));
  }
  let request = Request {
    query: b"cmd=getbundle",
    headers: &fields,
    body: b"",
  };
  answer(
    &cloned(),
    &request,
    HEADER_LINE,
    chooser,
    &mut Admission::new(ROOM),
  )
}

/// Format: the bundle2 capabilities a 7.2 client sends that a `getbundle` answer here reads.
const CLIENT_BLOB: &[u8] =
  b"HG20\nbookmarks\nchangegroup=01,02,03\nlistkeys\nobsmarkers=V0,V1\nphases=heads";

/// AC-HG.9, A-650 part 1: do ask `getbundle` of a `0.2` client that lists only changegroup version 01 (an abort), of a
/// client that reads `0.1` alone, and of a `0.2` client naming no engine this crate writes. Expect the abort a `0.2`
/// stream in `none`, uncompressed as 7.2 sends it, carrying 7.2's `ERROR:ABORT` part, with no engine chosen; and both
/// others `application/mercurial-0.1` in zlib, inflating to the very stream a `0.2` client is sent.
#[test]
fn an_abort_goes_uncompressed_and_a_client_without_engines_gets_zlib() {
  let v2: &[u8] = b"0.1 0.2 comp=zstd,zlib,none,bzip2 partial-pull";
  let mut chooser = First::at(3);
  let abort = getbundle_of(Some(v2), b"HG20\nchangegroup=01", &mut chooser);
  let (engine, stream) = inflated(&abort.body);
  let parts = crate::bundle2::Bundle2::decode(&stream, &mut Admission::new(ROOM)).unwrap();
  let kinds: Vec<&[u8]> = parts.top_level().map(|(_, part)| part.kind).collect();
  assert_eq!(
    (
      abort.media_type,
      engine.as_slice(),
      kinds,
      chooser.calls.len()
    ),
    (MEDIA_TYPE_V2, &b"none"[..], vec![&b"ERROR:ABORT"[..]], 0)
  );
  let whole = getbundle_of(Some(v2), CLIENT_BLOB, &mut chooser);
  let (_, expected) = inflated(&whole.body);
  for proto in [None, Some(&b"0.1 0.2 comp=lz4"[..])] {
    let ours = getbundle_of(proto, CLIENT_BLOB, &mut chooser);
    let mut stream = Vec::new();
    compression::inflate(
      Engine::Zlib,
      &ours.body,
      &mut Admission::new(ROOM),
      &mut stream,
    )
    .unwrap();
    assert_eq!(
      (ours.media_type, stream.as_slice()),
      (MEDIA_TYPE_V1, expected.as_slice())
    );
  }
  assert_eq!(chooser.calls.last(), Some(&(Media::V1, vec![Engine::Zlib])));
}

/// AC-HG.9, A-650 part 1: do give a chooser that answers bzip2 to a client reading only zstd and zlib, and one that
/// answers zstd at a level past its range. Expect each refused by kind and answered out of band, nothing compressed in
/// an engine the client cannot read.
#[test]
fn a_chooser_past_what_the_client_reads_is_refused() {
  let v2: &[u8] = b"0.1 0.2 comp=zstd,zlib";
  let bzip2 = Compression {
    engine: Engine::Bzip2,
    level: 9,
  };
  let ours = getbundle_of(Some(v2), CLIENT_BLOB, &mut Fixed(bzip2));
  assert_eq!(
    (ours.media_type, ours.refused),
    (
      MEDIA_TYPE_ERROR,
      Some(Refused::EngineNotRead {
        engine: Engine::Bzip2
      })
    )
  );
  let past = Compression {
    engine: Engine::Zstd,
    level: i32::MAX,
  };
  let ours = getbundle_of(Some(v2), CLIENT_BLOB, &mut Fixed(past));
  let refused = Refused::Compression(EncodeRefusal::Level {
    engine: Engine::Zstd,
    level: i32::MAX,
  });
  assert_eq!(
    (ours.media_type, ours.refused),
    (MEDIA_TYPE_ERROR, Some(refused))
  );
}

/// AC-HG.9, A-650 part 1: do send `known` without its `nodes`, arguments behind an `X-HgArgs-Post` longer than the
/// body, and `heads` to a request admitted three bytes. Expect each answered `application/hg-error`, its words ended by
/// a line end, naming its refusal by kind.
#[test]
fn requests_mercurial_would_misread_are_answered_out_of_band() {
  let served = cloned();
  let post: [(&[u8], &[u8]); 1] = [(b"X-HgArgs-Post", b"99")];
  let cases: [(&[u8], &[(&[u8], &[u8])], u64, Refused); 3] = [
    (
      b"cmd=known",
      &[],
      ROOM,
      Refused::Http(http::Refusal::MissingArgument { position: 0 }),
    ),
    (
      b"cmd=heads",
      &post,
      ROOM,
      Refused::Http(http::Refusal::PostArguments),
    ),
    (
      b"cmd=heads",
      &[],
      3,
      Refused::Http(http::Refusal::PastAdmission(PastAdmission {
        asked: 8,
        left: 3,
      })),
    ),
  ];
  for (query, headers, room, refused) in cases {
    let request = Request {
      query,
      headers,
      body: b"short",
    };
    let ours = answer(
      &served,
      &request,
      HEADER_LINE,
      &mut First::at(3),
      &mut Admission::new(room),
    );
    assert_eq!(
      (ours.media_type, ours.refused),
      (MEDIA_TYPE_ERROR, Some(refused))
    );
    assert!(ours.body.ends_with(b"\n") && ours.body.len() > 1);
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// AC-HG.9, A-650 part 1: do send arbitrary bytes as a request's query, its argument and protocol headers and its
  /// body, after a generated command. Expect an answer, never a panic, in one of the three media types, its `0.2`
  /// bodies naming an engine the client listed.
  #[test]
  fn arbitrary_requests_are_answered_in_a_media_type(
    command in 0_usize..6,
    bytes in proptest::collection::vec(any::<u8>(), 0..96),
  ) {
    let commands = ["heads", "known", "batch", "getbundle", "listkeys", "capabilities"];
    let query = [b"cmd=".as_slice(), commands[command].as_bytes(), b"&", bytes.as_slice()].concat();
    let headers: [(&[u8], &[u8]); 2] = [(b"X-HgArg-1", &bytes), (b"X-HgProto-1", &bytes)];
    let request = Request { query: &query, headers: &headers, body: &bytes };
    let mut room = Admission::new(ROOM);
    let ours = answer(&cloned(), &request, HEADER_LINE, &mut First::at(1), &mut room);
    prop_assert!([MEDIA_TYPE_V1, MEDIA_TYPE_V2, MEDIA_TYPE_ERROR].contains(&ours.media_type));
    if ours.media_type == MEDIA_TYPE_V2 {
      let (engine, _) = http::decode_v2(&ours.body).unwrap();
      prop_assert!(Engine::for_wire_name(engine).is_some());
    }
  }
}

/// A-650 part 1: do ask what `unbundle`, `pushkey`, `heads` and a name 7.2's table lacks need, and which command a query
/// naming `cmd` twice, escaped, names. Expect a push for the first two, a pull for `heads`, none for the last (no wire
/// request), and the last `cmd`, decoded.
#[test]
fn the_permission_a_command_needs_is_mercurials() {
  let needs: Vec<Option<Permission>> = [&b"unbundle"[..], b"pushkey", b"heads", b"frobnicate"]
    .iter()
    .map(|name| permission(name))
    .collect();
  let named = command(b"cmd=heads&cmd=list%6Beys", &mut Admission::new(ROOM));
  assert_eq!(
    (needs, named),
    (
      vec![
        Some(Permission::Push),
        Some(Permission::Push),
        Some(Permission::Pull),
        None
      ],
      Ok(Some(b"listkeys".to_vec()))
    )
  );
}
