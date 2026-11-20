use std::convert::Infallible;

use proptest::prelude::*;
use slates_vcs::ContentStore;

use super::{Argument, Info, ObjectSizes, Size, Stored, answer, decode_answer, encode_answer};
use crate::object_name::{HexRefusal, ObjectFormat, ObjectId, read_header};
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

/// A repository's sizes as a plain list: what the answer asks of a store.
struct Sizes(Vec<(ObjectId, u64)>);

impl ObjectSizes for Sizes {
  fn size(&self, id: &ObjectId) -> Option<u64> {
    self
      .0
      .iter()
      .find(|(known, _)| known == id)
      .map(|(_, size)| *size)
  }
}

/// The SHA-1 repository's objects with their sizes, as `objects.txt` lists them.
fn sha1_sizes() -> Sizes {
  Sizes(sizes(ObjectFormat::Sha1, rest::OBJECTS_TXT))
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

/// What [`answer`] writes for `arguments` from `sizes`, in `format`.
fn answered(
  arguments: &[Line<Argument>],
  format: ObjectFormat,
  sizes: &impl ObjectSizes,
) -> Vec<u8> {
  let mut out = Vec::new();
  answer(arguments, format, sizes, &mut out).unwrap();
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
  let sha256 = Sizes(sizes(ObjectFormat::Sha256, rest::SHA256_OBJECTS_TXT));
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
      answered(&arguments(request), ObjectFormat::Sha1, &sha1_sizes()),
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
  let object = |hex: &str, size| Info::Object {
    id: id(ObjectFormat::Sha1, hex),
    size,
  };
  assert_eq!(
    infos,
    [
      Info::Size,
      object("814f4a422927b82f5f8a43f8fab6d3839e3983f2", Size::Bytes(8)),
      object("ebe21f4c837b47ecf7ca7946682ded94e2f6b4fb", Size::Bytes(60)),
      object("2e01529673a750e4b001c538bc95d5a4b7c9d7e3", Size::Bytes(198)),
      object(UNKNOWN, Size::Unknown),
      object(
        "224a210720bd8e17e8e582f10cff2b95cd5754f9",
        Size::Bytes(1071)
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
/// branch to its tip, and answer the request for sizes from the store's rows. Expect: every byte stock git 2.50.1
/// answered, the object the store lacks answered with its space alone.
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
      answered(&arguments, ObjectFormat::Sha1, &sha1_sizes()),
      gits.last().map(|line| line.value),
    ),
    (
      vec![Argument::Size, Argument::Oid(blob)],
      rest::OBJECT_INFO_UPPER_RESPONSE.to_ascii_lowercase(),
      Some(Info::Object {
        id: blob,
        size: Size::Bytes(8),
      }),
    )
  );
}

/// A-624, refused by kind: do decode `object-info` answers out of their grammar, one fault each. Expect: each refused at
/// the line at fault: a size with no `size` line before it, an object line without a size after `size`, `size` after
/// an object line, a size not in canonical decimal, an id that is none, a delimiter, the bytes ending early or going on.
#[test]
fn an_answer_out_of_its_grammar_is_refused_by_kind() {
  let blob = "814f4a422927b82f5f8a43f8fab6d3839e3983f2";
  let lines = |texts: &[String]| {
    let mut packets: Vec<Packet<'_>> = texts
      .iter()
      .map(|text| Packet::Data(text.as_bytes()))
      .collect();
    packets.push(Packet::Flush);
    wire(&packets)
  };
  let second = to_u64(4 + 4);
  let cases = [
    (lines(&[format!("{blob} 8")]), Refusal::Line { at: 0 }),
    (
      lines(&["size".into(), blob.into()]),
      Refusal::Line { at: second },
    ),
    (
      lines(&[blob.into(), "size".into()]),
      Refusal::ObjectId {
        at: to_u64(4 + 40),
        refusal: HexRefusal::Length { len: 4 },
      },
    ),
    (
      lines(&["size".into(), format!("{blob} 08")]),
      Refusal::Number { at: second },
    ),
    (
      lines(&["size".into(), format!("{blob} 8 ")]),
      Refusal::Number { at: second },
    ),
    (
      lines(&["size".into(), format!("{blob}x 8")]),
      Refusal::ObjectId {
        at: second,
        refusal: HexRefusal::Length { len: 41 },
      },
    ),
    (
      lines(&["type".into()]),
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

/// A-624, refused by kind: do decode `object-info` requests whose arguments are out of their grammar, one fault each,
/// and the arguments git's master adds after 2.50.1. Expect: each refused at its line: `oid` with no id, an id too short,
/// bytes after an id, `size` with a value, `type`, a word git does not know.
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
    ("type", Refusal::UnknownArgument { at }),
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
  let object = |size| Line::bare(Info::Object { id: blob, size });
  let attempt = |write: &dyn Fn(&mut Vec<u8>) -> Result<(), Refusal>| {
    let mut out = Vec::new();
    let result = write(&mut out);
    (result, out.len())
  };
  assert_eq!(
    (
      attempt(&|out| encode_answer(&[object(Size::Bytes(8))], ObjectFormat::Sha1, out)),
      attempt(&|out| encode_answer(
        &[Line::bare(Info::Size), object(Size::NotAsked)],
        ObjectFormat::Sha1,
        out
      )),
      attempt(&|out| encode_answer(&[object(Size::NotAsked)], ObjectFormat::Sha256, out)),
      attempt(&|out| answer(
        &[Line::bare(Argument::Size), wide],
        ObjectFormat::Sha1,
        &sha1_sizes(),
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

/// An object line's id bytes, its size if asked, whether the store holds it, and its size there.
fn any_object() -> impl Strategy<Value = ([u8; 20], bool, u64)> {
  (any::<[u8; 20]>(), any::<bool>(), any::<u64>())
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-624: do answer any request (sizes asked or not, any objects, held or not) from a store, and decode the answer.
  /// Expect: nothing but the flush when no object is named; otherwise `size` where it was asked for, then each object
  /// in the order named, with its size, the space alone where the store lacks it, or the id alone where no size was
  /// asked for; and the answer encodes back to its bytes.
  #[test]
  fn any_request_is_answered_as_git_answers_it(
    asked in any::<bool>(),
    objects in proptest::collection::vec(any_object(), 0..8),
  ) {
    let ids: Vec<ObjectId> = objects
      .iter()
      .map(|(bytes, _, _)| ObjectId::from_bytes(ObjectFormat::Sha1, bytes).unwrap())
      .collect();
    let store = Sizes(
      ids
        .iter()
        .zip(&objects)
        .filter(|(_, (_, held, _))| *held)
        .map(|(id, (_, _, size))| (*id, *size))
        .collect(),
    );
    let mut arguments: Vec<Line<Argument>> = ids.iter().map(|id| Line::lf(Argument::Oid(*id))).collect();
    if asked {
      arguments.insert(0, Line::lf(Argument::Size));
    }
    let bytes = answered(&arguments, ObjectFormat::Sha1, &store);
    let read = decode_answer(&bytes, ObjectFormat::Sha1, &mut room()).unwrap();
    let mut expected = Vec::new();
    if asked && !ids.is_empty() {
      expected.push(Info::Size);
    }
    for id in &ids {
      let size = match (asked, store.size(id)) {
        (false, _) => Size::NotAsked,
        (true, None) => Size::Unknown,
        (true, Some(bytes)) => Size::Bytes(bytes),
      };
      expected.push(Info::Object { id: *id, size });
    }
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
