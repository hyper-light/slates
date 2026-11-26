use std::convert::Infallible;

use proptest::prelude::*;
use slates_vcs::ContentStore;

use super::{
  Argument, Attr, Info, ObjectInfo, ObjectMeta, Stored, Values, answer, decode_answer,
  encode_answer,
};
use crate::object_name::{HexRefusal, Kind, ObjectFormat, ObjectId, read_header};
use crate::pktline::Packet;
use crate::store::{Entries, Incoming, Push, Repository};
use crate::v2::request::{self, Arguments, Command, Request};
use crate::v2::tests::{room, wire};
use crate::v2::{Line, Refusal};
use crate::v2_rest_fixtures::{self as rest, id, sizes};
use crate::{Admission, to_u64};

/// Shape: the window the test's content store holds content in.
const WINDOW: usize = 1 << 10;

/// Shape: the test's content budget, room to spare.
const BUDGET: u64 = 1 << 26;

/// The id the script's `printf 'not in the server\n' | git hash-object --stdin` made: an object the server never had.
const UNKNOWN: &str = "86e52f206da93ad5319625df790d3a5ed9f57f60";

/// A repository's object metadata as a plain list: what the answer asks of a store.
struct Objects(Vec<(ObjectId, ObjectMeta)>);

impl ObjectInfo for Objects {
  fn info(&self, id: &ObjectId) -> Option<ObjectMeta> {
    self
      .0
      .iter()
      .find(|(known, _)| known == id)
      .map(|(_, meta)| *meta)
  }
}

/// Parses a fixture `objects.txt` (`<hex> <type> <size>` per line) into `(id, ObjectMeta)` entries.
fn fixture_meta(format: ObjectFormat, txt: &[u8]) -> Vec<(ObjectId, ObjectMeta)> {
  core::str::from_utf8(txt)
    .unwrap()
    .lines()
    .map(|line| {
      let mut parts = line.split(' ');
      let oid = id(format, parts.next().unwrap());
      let kind = Kind::from_name(parts.next().unwrap().as_bytes()).unwrap();
      let size = parts.next().unwrap().parse().unwrap();
      (oid, ObjectMeta { size, kind })
    })
    .collect()
}

/// The SHA-1 repository's objects with their sizes and types, as `objects.txt` lists them.
fn sha1_objects() -> Objects {
  Objects(fixture_meta(ObjectFormat::Sha1, rest::OBJECTS_TXT))
}

/// The `object-info` arguments of a request stock git answered.
fn arguments(stream: &[u8]) -> Vec<Line<Argument>> {
  match request::decode(stream, &mut room()).unwrap() {
    Request::Command(request) => match request.arguments {
      Arguments::ObjectInfo(arguments) => arguments,
      other => panic!("not object-info: {other:?}"),
    },
    Request::End => panic!("no command"),
  }
}

/// What [`answer`] writes for `arguments` from `objects`, in `format`.
fn answered(
  arguments: &[Line<Argument>],
  format: ObjectFormat,
  objects: &impl ObjectInfo,
) -> Vec<u8> {
  let mut out = Vec::new();
  answer(arguments, format, objects, &mut out).unwrap();
  out
}

/// A-624, golden vectors: do decode the `object-info` requests stock git 2.50.1 answered, in SHA-1 and in SHA-256.
/// Expect: each the command `object-info` with its arguments as written: `size` and each `oid`, `oid`s alone, `size`
/// alone.
#[test]
fn stock_gits_object_info_requests_decode_to_their_arguments() {
  let sha1 = |hex: &str| Argument::Oid(id(ObjectFormat::Sha1, hex));
  let names: Vec<ObjectId> = sizes(ObjectFormat::Sha1, rest::OBJECTS_TXT)
    .into_iter()
    .map(|(id, _)| id)
    .collect();
  let read =
    |stream: &[u8]| -> Vec<Argument> { arguments(stream).iter().map(|line| line.value).collect() };
  let sha256 = sizes(ObjectFormat::Sha256, rest::SHA256_OBJECTS_TXT);
  assert_eq!(
    (
      read(rest::OBJECT_INFO_SIZES_REQUEST),
      read(rest::OBJECT_INFO_NO_SIZE_REQUEST),
      read(rest::OBJECT_INFO_NO_OID_REQUEST),
      read(rest::OBJECT_INFO_SHA256_REQUEST),
    ),
    (
      vec![
        Argument::Size,
        Argument::Oid(names[3]),
        Argument::Oid(names[2]),
        Argument::Oid(names[0]),
        sha1(UNKNOWN),
        Argument::Oid(names[4]),
      ],
      vec![Argument::Oid(names[3]), Argument::Oid(names[0])],
      vec![Argument::Size],
      vec![
        Argument::Size,
        Argument::Oid(sha256[2].0),
        Argument::Oid(sha256[0].0)
      ],
    )
  );
}

/// A-624, golden vectors: do answer each `object-info` request stock git 2.50.1 answered, from the same objects'
/// sizes, and decode git's answer. Expect: every byte git wrote, in SHA-1 and in SHA-256: `size` then each object's
/// line, the object the server lacks with its space alone; ids alone without `size`; the flush alone with no id; every
/// line without an LF. And git's answer read as those lines.
#[test]
fn each_answer_is_the_one_stock_git_wrote_from_the_same_objects() {
  let sha256 = Objects(fixture_meta(ObjectFormat::Sha256, rest::SHA256_OBJECTS_TXT));
  for (name, request, response) in [
    (
      "sizes",
      rest::OBJECT_INFO_SIZES_REQUEST,
      rest::OBJECT_INFO_SIZES_RESPONSE,
    ),
    (
      "no size",
      rest::OBJECT_INFO_NO_SIZE_REQUEST,
      rest::OBJECT_INFO_NO_SIZE_RESPONSE,
    ),
    (
      "no oid",
      rest::OBJECT_INFO_NO_OID_REQUEST,
      rest::OBJECT_INFO_NO_OID_RESPONSE,
    ),
  ] {
    assert_eq!(
      answered(&arguments(request), ObjectFormat::Sha1, &sha1_objects()),
      response,
      "{name}"
    );
  }
  assert_eq!(
    answered(
      &arguments(rest::OBJECT_INFO_SHA256_REQUEST),
      ObjectFormat::Sha256,
      &sha256
    ),
    rest::OBJECT_INFO_SHA256_RESPONSE
  );
  let lines = decode_answer(
    rest::OBJECT_INFO_SIZES_RESPONSE,
    ObjectFormat::Sha1,
    &mut room(),
  )
  .unwrap();
  let infos: Vec<Info> = lines.iter().map(|line| line.value).collect();
  let object = |hex: &str, values| Info::Object {
    id: id(ObjectFormat::Sha1, hex),
    values,
  };
  assert_eq!(
    infos,
    [
      Info::Attr(Attr::Size),
      object("814f4a422927b82f5f8a43f8fab6d3839e3983f2", Values::Size(8)),
      object("ebe21f4c837b47ecf7ca7946682ded94e2f6b4fb", Values::Size(60)),
      object(
        "2e01529673a750e4b001c538bc95d5a4b7c9d7e3",
        Values::Size(198)
      ),
      object(UNKNOWN, Values::Unknown),
      object(
        "224a210720bd8e17e8e582f10cff2b95cd5754f9",
        Values::Size(1071)
      ),
    ]
  );
}

/// The loose objects `objects.bin` holds, handed to the store one after another as an import hands them, each whole
/// (a delta chain's depth of zero).
struct Loose<'a> {
  bytes: &'a [u8],
  at: usize,
}

impl Entries for Loose<'_> {
  type Refused = Infallible;

  fn next_entry(&mut self) -> Option<Result<Incoming<'_>, Infallible>> {
    let rest = self.bytes.get(self.at..).filter(|rest| !rest.is_empty())?;
    let (header, took) = read_header(rest, &mut Admission::new(u64::MAX)).ok()?;
    let end = took.checked_add(usize::try_from(header.size).ok()?)?;
    let content = rest.get(took..end)?;
    self.at = self.at.checked_add(end)?;
    Some(Ok(Incoming {
      kind: header.kind,
      claimed: None,
      content,
      depth: 0,
    }))
  }
}

/// A-624, the store as the answer's source: do push every object of the SHA-1 repository into A-614's store, move a
/// branch to its tip, and answer `size`, `type`, and `size`+`type` requests from the store's rows. Expect: every byte
/// stock git 2.50.1 / 2.56.1 answered, the object the store lacks answered with its space alone.
#[test]
fn the_store_answers_object_info_as_stock_git_did() {
  let mut content = ContentStore::new(BUDGET, WINDOW).unwrap();
  let mut repository = Repository::new(ObjectFormat::Sha1);
  let mut push = Push::new(ObjectFormat::Sha1);
  let mut loose = Loose {
    bytes: rest::OBJECTS,
    at: 0,
  };
  push
    .admit_all(&mut content, &repository.naming(), &mut loose)
    .unwrap();
  repository.seal(&content, &mut push).unwrap();
  let tip = id(
    ObjectFormat::Sha1,
    "2e01529673a750e4b001c538bc95d5a4b7c9d7e3",
  );
  let moved = repository.admit(&content, &push, 0, Some(tip)).unwrap();
  repository
    .commit(&mut content, &mut push, &[moved])
    .unwrap();
  let stored = Stored {
    repository: &repository,
    content: &content,
  };
  assert_eq!(
    answered(
      &arguments(rest::OBJECT_INFO_SIZES_REQUEST),
      ObjectFormat::Sha1,
      &stored
    ),
    rest::OBJECT_INFO_SIZES_RESPONSE
  );
  let blob = id(
    ObjectFormat::Sha1,
    "814f4a422927b82f5f8a43f8fab6d3839e3983f2",
  );
  let tree = id(
    ObjectFormat::Sha1,
    "ebe21f4c837b47ecf7ca7946682ded94e2f6b4fb",
  );
  let unknown = id(ObjectFormat::Sha1, UNKNOWN);
  let both = answered(
    &[
      Line::lf(Argument::Type),
      Line::lf(Argument::Size),
      Line::lf(Argument::Oid(blob)),
      Line::lf(Argument::Oid(tree)),
      Line::lf(Argument::Oid(tip)),
      Line::lf(Argument::Oid(unknown)),
    ],
    ObjectFormat::Sha1,
    &stored,
  );
  assert_eq!(
    both,
    wire(&[
      Packet::Data(b"size"),
      Packet::Data(b"type"),
      Packet::Data(b"814f4a422927b82f5f8a43f8fab6d3839e3983f2 8 blob"),
      Packet::Data(b"ebe21f4c837b47ecf7ca7946682ded94e2f6b4fb 60 tree"),
      Packet::Data(b"2e01529673a750e4b001c538bc95d5a4b7c9d7e3 198 commit"),
      Packet::Data(format!("{UNKNOWN} ").as_bytes()),
      Packet::Flush,
    ])
  );
}

/// Stock Git 2.56.1 `object-info` `type` and `size`+`type` wire parity: do decode requests asking
/// for `type`, `size`+`type`, `type`+`size` (reversed), and `type` with no OIDs; answer each and
/// round-trip both `(size, type)` and `(type, size)` response wire encodings. Expect: `size` always
/// precedes `type` in server responses regardless of request order, unknown OIDs always emit
/// `<oid> `, no-OID requests emit flush alone, and both header orders decode and encode cleanly.
#[test]
fn type_and_size_type_requests_and_answers_match_stock_git_2_56() {
  let blob_hex = "814f4a422927b82f5f8a43f8fab6d3839e3983f2";
  let commit_hex = "2e01529673a750e4b001c538bc95d5a4b7c9d7e3";
  let blob = id(ObjectFormat::Sha1, blob_hex);
  let commit = id(ObjectFormat::Sha1, commit_hex);
  let unknown = id(ObjectFormat::Sha1, UNKNOWN);
  let objects = sha1_objects();

  let type_req = wire(&[
    Packet::Data(b"command=object-info\n"),
    Packet::Delim,
    Packet::Data(b"type\n"),
    Packet::Data(format!("oid {blob_hex}\n").as_bytes()),
    Packet::Data(format!("oid {commit_hex}\n").as_bytes()),
    Packet::Data(format!("oid {UNKNOWN}\n").as_bytes()),
    Packet::Flush,
  ]);
  let decoded_type = arguments(&type_req);
  assert_eq!(
    decoded_type
      .iter()
      .map(|line| line.value)
      .collect::<Vec<_>>(),
    [
      Argument::Type,
      Argument::Oid(blob),
      Argument::Oid(commit),
      Argument::Oid(unknown),
    ]
  );
  let type_bytes = answered(&decoded_type, ObjectFormat::Sha1, &objects);
  assert_eq!(
    type_bytes,
    wire(&[
      Packet::Data(b"type"),
      Packet::Data(format!("{blob_hex} blob").as_bytes()),
      Packet::Data(format!("{commit_hex} commit").as_bytes()),
      Packet::Data(format!("{UNKNOWN} ").as_bytes()),
      Packet::Flush,
    ])
  );
  assert_eq!(
    answered(&[Line::lf(Argument::Type)], ObjectFormat::Sha1, &objects),
    wire(&[Packet::Flush])
  );
  // Decode and round-trip a server response announcing `type` before `size`.
  let type_then_size = wire(&[
    Packet::Data(b"type"),
    Packet::Data(b"size"),
    Packet::Data(format!("{blob_hex} blob 8").as_bytes()),
    Packet::Data(format!("{UNKNOWN} ").as_bytes()),
    Packet::Flush,
  ]);
  let parsed = decode_answer(&type_then_size, ObjectFormat::Sha1, &mut room()).unwrap();
  assert_eq!(
    parsed.iter().map(|line| line.value).collect::<Vec<_>>(),
    [
      Info::Attr(Attr::Type),
      Info::Attr(Attr::Size),
      Info::Object {
        id: blob,
        values: Values::TypeSize(Kind::Blob, 8),
      },
      Info::Object {
        id: unknown,
        values: Values::Unknown,
      },
    ]
  );
  let mut encoded = Vec::new();
  encode_answer(&parsed, ObjectFormat::Sha1, &mut encoded).unwrap();
  assert_eq!(encoded, type_then_size);
}

/// A-624 (a server reads an id as git's server does, either case, and writes git's form): do decode the request naming a
/// blob in upper case, answer it, and decode git's answer, which echoes the id as its client wrote it. Expect: the id
/// read as the blob's; the answer every byte git wrote but the id, written in lower case; git's answer read as the same
/// line.
#[test]
fn an_upper_case_id_is_read_and_answered_in_lower_case() {
  let arguments = arguments(rest::OBJECT_INFO_UPPER_REQUEST);
  let blob = id(
    ObjectFormat::Sha1,
    "814f4a422927b82f5f8a43f8fab6d3839e3983f2",
  );
  let gits = decode_answer(
    rest::OBJECT_INFO_UPPER_RESPONSE,
    ObjectFormat::Sha1,
    &mut room(),
  )
  .unwrap();
  assert_eq!(
    (
      arguments.iter().map(|line| line.value).collect::<Vec<_>>(),
      answered(&arguments, ObjectFormat::Sha1, &sha1_objects()),
      gits.last().map(|line| line.value),
    ),
    (
      vec![Argument::Size, Argument::Oid(blob)],
      rest::OBJECT_INFO_UPPER_RESPONSE.to_ascii_lowercase(),
      Some(Info::Object {
        id: blob,
        values: Values::Size(8),
      }),
    )
  );
}

/// Builds a pkt-line answer from `texts` followed by a flush.
fn answer_wire(texts: &[String]) -> Vec<u8> {
  let mut packets: Vec<Packet<'_>> = texts
    .iter()
    .map(|text| Packet::Data(text.as_bytes()))
    .collect();
  packets.push(Packet::Flush);
  wire(&packets)
}

/// A-624, refused by kind: do decode `object-info` answers out of their grammar, one fault each. Expect: each refused at
/// the line at fault: a size with no `size` line before it, an object line without a size after `size`, duplicate
/// attribute headers, `size` after an object line, a size not in canonical decimal, an invalid type name, a missing
/// second column after `size`+`type`, an unknown attribute word, an id that is none, a delimiter, the bytes ending
/// early or going on.
#[test]
fn an_answer_out_of_its_grammar_is_refused_by_kind() {
  let blob = "814f4a422927b82f5f8a43f8fab6d3839e3983f2";
  let second = to_u64(4 + 4);
  let third = to_u64(4 + 4 + 4 + 4);
  let cases = [
    (answer_wire(&[format!("{blob} 8")]), Refusal::Line { at: 0 }),
    (
      answer_wire(&["size".into(), blob.into()]),
      Refusal::Line { at: second },
    ),
    (
      answer_wire(&["size".into(), "size".into()]),
      Refusal::Line { at: second },
    ),
    (
      answer_wire(&["type".into(), "type".into()]),
      Refusal::Line { at: second },
    ),
    (
      answer_wire(&["type".into(), format!("{blob} bogus")]),
      Refusal::Line { at: second },
    ),
    (
      answer_wire(&["size".into(), "type".into(), format!("{blob} 8")]),
      Refusal::Line { at: third },
    ),
    (
      answer_wire(&[blob.into(), "size".into()]),
      Refusal::ObjectId {
        at: to_u64(4 + 40),
        refusal: HexRefusal::Length { len: 4 },
      },
    ),
    (
      answer_wire(&["size".into(), format!("{blob} 08")]),
      Refusal::Number { at: second },
    ),
    (
      answer_wire(&["size".into(), format!("{blob} 8 ")]),
      Refusal::Number { at: second },
    ),
    (
      answer_wire(&["size".into(), format!("{blob}x 8")]),
      Refusal::ObjectId {
        at: second,
        refusal: HexRefusal::Length { len: 41 },
      },
    ),
    (
      answer_wire(&["kind".into()]),
      Refusal::ObjectId {
        at: 0,
        refusal: HexRefusal::Length { len: 4 },
      },
    ),
    (
      wire(&[Packet::Delim, Packet::Flush]),
      Refusal::Misplaced { at: 0 },
    ),
    (
      wire(&[Packet::Flush, Packet::Flush]),
      Refusal::Trailing { at: 4 },
    ),
    (wire(&[]), Refusal::Unfinished { at: 0 }),
  ];
  for (bytes, refusal) in cases {
    assert_eq!(
      decode_answer(&bytes, ObjectFormat::Sha1, &mut room()),
      Err(refusal),
      "{:?}",
      bytes.escape_ascii().to_string()
    );
  }
}

/// A-624, refused by kind: do decode `object-info` requests whose arguments are out of their grammar, one fault each.
/// Expect: each refused at its line: `oid` with no id, an id too short, bytes after an id, `size` or `type` with a
/// value, a word git does not know.
#[test]
fn an_argument_out_of_its_grammar_is_refused_by_kind() {
  let request = |argument: &str| {
    let line = format!("{argument}\n");
    wire(&[
      Packet::Data(b"command=object-info\n"),
      Packet::Delim,
      Packet::Data(line.as_bytes()),
      Packet::Flush,
    ])
  };
  let at = to_u64(4 + "command=object-info\n".len() + 4);
  let blob = "814f4a422927b82f5f8a43f8fab6d3839e3983f2";
  for (argument, refusal) in [
    ("oid", Refusal::UnknownArgument { at }),
    (
      "oid ",
      Refusal::ObjectId {
        at,
        refusal: HexRefusal::Length { len: 0 },
      },
    ),
    (
      &format!("oid {}", blob.get(..39).unwrap()),
      Refusal::ObjectId {
        at,
        refusal: HexRefusal::Length { len: 39 },
      },
    ),
    (
      &format!("oid {blob} x"),
      Refusal::ObjectId {
        at,
        refusal: HexRefusal::Length { len: 42 },
      },
    ),
    ("size 1", Refusal::UnknownArgument { at }),
    ("type 1", Refusal::UnknownArgument { at }),
    ("types", Refusal::UnknownArgument { at }),
    ("sizes", Refusal::UnknownArgument { at }),
  ] {
    assert_eq!(
      request::decode(&request(argument), &mut room()),
      Err(refusal),
      "{argument:?}"
    );
  }
}

/// A-624: do write answers that break the grammar, and answer a request whose id is in another format. Expect: each
/// refused by kind, nothing written.
#[test]
fn a_refused_answer_writes_nothing() {
  let blob = id(
    ObjectFormat::Sha1,
    "814f4a422927b82f5f8a43f8fab6d3839e3983f2",
  );
  let wide = Line::bare(Argument::Oid(id(
    ObjectFormat::Sha256,
    "a4ed1f355afb02d88cd291d0e4463910c5061ece48a49aa2b1539b9af973b286",
  )));
  let object = |values| Line::bare(Info::Object { id: blob, values });
  let attempt = |write: &dyn Fn(&mut Vec<u8>) -> Result<(), Refusal>| {
    let mut out = Vec::new();
    let result = write(&mut out);
    (result, out.len())
  };
  assert_eq!(
    (
      attempt(&|out| encode_answer(&[object(Values::Size(8))], ObjectFormat::Sha1, out)),
      attempt(&|out| encode_answer(
        &[Line::bare(Info::Attr(Attr::Size)), object(Values::NotAsked)],
        ObjectFormat::Sha1,
        out
      )),
      attempt(&|out| encode_answer(&[object(Values::NotAsked)], ObjectFormat::Sha256, out)),
      attempt(&|out| answer(
        &[Line::bare(Argument::Size), wide],
        ObjectFormat::Sha1,
        &sha1_objects(),
        out
      )),
    ),
    (
      (Err(Refusal::Line { at: 0 }), 0),
      (Err(Refusal::Line { at: 8 }), 0),
      (
        Err(Refusal::ObjectId {
          at: 0,
          refusal: HexRefusal::Length { len: 40 }
        }),
        0
      ),
      (
        Err(Refusal::ObjectId {
          at: 8,
          refusal: HexRefusal::Length { len: 64 }
        }),
        0
      ),
    )
  );
}

/// Maps selector `0..4` to a Git [`Kind`] without `prop_oneof!` (R2).
fn kind_from_tag(tag: u8) -> Kind {
  match tag {
    0 => Kind::Commit,
    1 => Kind::Tree,
    2 => Kind::Blob,
    _ => Kind::Tag,
  }
}

/// An object line's id bytes, whether the store holds it, its size, and its kind selector.
fn any_object() -> impl Strategy<Value = ([u8; 20], bool, u64, u8)> {
  (any::<[u8; 20]>(), any::<bool>(), any::<u64>(), 0u8..4)
}

/// Builds expected [`Info`] lines for a proptest `object-info` request.
fn expected_infos(
  asked_size: bool,
  asked_type: bool,
  ids: &[ObjectId],
  store: &Objects,
) -> Vec<Info> {
  if ids.is_empty() {
    return Vec::new();
  }
  let mut expected = Vec::new();
  if asked_size {
    expected.push(Info::Attr(Attr::Size));
  }
  if asked_type {
    expected.push(Info::Attr(Attr::Type));
  }
  expected.extend(ids.iter().map(|&id| {
    let values = match (asked_size, asked_type, store.info(&id)) {
      (false, false, _) => Values::NotAsked,
      (_, _, None) => Values::Unknown,
      (true, true, Some(meta)) => Values::SizeType(meta.size, meta.kind),
      (true, false, Some(meta)) => Values::Size(meta.size),
      (false, true, Some(meta)) => Values::Type(meta.kind),
    };
    Info::Object { id, values }
  }));
  expected
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-624: do answer any request (`size` and/or `type` asked or not, any objects, held or not) from a store, and
  /// decode the answer. Expect: nothing but the flush when no object is named; otherwise `size` and/or `type` headers
  /// in Git 2.56.1's fixed order, then each object in the order named, with its values, the space alone where the
  /// store lacks it, or the id alone where no attribute was asked for; and the answer encodes back to its bytes.
  #[test]
  fn any_request_is_answered_as_git_answers_it(
    asked_size in any::<bool>(),
    asked_type in any::<bool>(),
    objects in proptest::collection::vec(any_object(), 0..8),
  ) {
    let ids: Vec<ObjectId> = objects
      .iter()
      .map(|(bytes, _, _, _)| ObjectId::from_bytes(ObjectFormat::Sha1, bytes).unwrap())
      .collect();
    let store = Objects(
      ids
        .iter()
        .zip(&objects)
        .filter(|(_, (_, held, _, _))| *held)
        .map(|(id, (_, _, size, tag))| (*id, ObjectMeta { size: *size, kind: kind_from_tag(*tag) }))
        .collect(),
    );
    let mut arguments: Vec<Line<Argument>> = ids.iter().map(|id| Line::lf(Argument::Oid(*id))).collect();
    if asked_type {
      arguments.insert(0, Line::lf(Argument::Type));
    }
    if asked_size {
      arguments.insert(0, Line::lf(Argument::Size));
    }
    let bytes = answered(&arguments, ObjectFormat::Sha1, &store);
    let read = decode_answer(&bytes, ObjectFormat::Sha1, &mut room()).unwrap();
    let expected = expected_infos(asked_size, asked_type, &ids, &store);
    let mut again = Vec::new();
    encode_answer(&read, ObjectFormat::Sha1, &mut again).unwrap();
    prop_assert_eq!(
      (read.iter().map(|line| line.value).collect::<Vec<_>>(), again),
      (expected, bytes)
    );
  }
}

/// A-624: the commands' names in a request. Do: decode an `object-info` request with no delimiter and no argument, as
/// `serve.c` takes a command whose flush follows its head. Expect: `object-info` with no arguments, written back.
#[test]
fn an_object_info_request_without_arguments_is_read_and_written_back() {
  let stream = wire(&[Packet::Data(b"command=object-info\n"), Packet::Flush]);
  let read = request::decode(&stream, &mut room()).unwrap();
  let mut out = Vec::new();
  request::encode(&read, &mut out).unwrap();
  let Request::Command(command) = read else {
    panic!("no command");
  };
  assert_eq!(
    (command.command(), command.arguments.len(), out),
    (Command::ObjectInfo, 0, stream)
  );
}
