use proptest::prelude::*;

use super::{
  Bundle, BundleList, Bundles, Heuristic, Mode, Pair, answer, decode_answer, encode_answer,
};
use crate::object_name::ObjectFormat;
use crate::pktline::{self, Packet};
use crate::v2::fetch::{self, Argument as F};
use crate::v2::request::{self, Arguments, Command, Head, Request};
use crate::v2::tests::{room, wire};
use crate::v2::{Capability, Ending, Line, Refusal, advertisement, ls_refs};
use crate::v2_rest_fixtures::{self as rest, id};
use crate::{Admission, to_u64};

/// A repository's bundle list as a value: what the answer asks of a store.
struct Offered<'a>(Option<BundleList<'a>>);

impl Bundles for Offered<'_> {
  fn bundle_list(&self) -> Option<BundleList<'_>> {
    self.0
  }
}

/// What [`answer`] writes for `offered`.
fn answered(offered: &Offered<'_>) -> Vec<u8> {
  let mut out = Vec::new();
  answer(offered, &mut out).unwrap();
  out
}

/// The bundle the script's server offered: the first commit's bundle, at a URI relative to the server's.
const BASE: Bundle<'static> = Bundle {
  id: b"base",
  uri: b"../base.bundle",
  creation_token: Some(1),
};

/// The list the script's server offered.
const LIST: BundleList<'static> = BundleList {
  mode: Mode::All,
  heuristic: Some(Heuristic::CreationToken),
  bundles: &[BASE],
};

/// `key=value`, as a line without an LF, as git's server writes it.
fn pair<'a>(key: &'a [u8], value: &'a [u8]) -> Line<Pair<'a>> {
  Line::bare(Pair { key, value })
}

/// A-624, golden vectors: do decode the `bundle-uri` request stock git 2.50.1 answered, written as git's client writes
/// one, and its answer; and answer the request from the list the script's server held. Expect: the request's head its
/// capability before its command, then a delimiter and no argument; the answer read as the server's five keys in its
/// order, every line without an LF; and the list answered with every byte git wrote.
#[test]
fn stock_gits_exchange_reads_as_its_lines_and_the_list_answers_it_byte_for_byte() {
  let Request::Command(read) = request::decode(rest::BUNDLE_URI_REQUEST, &mut room()).unwrap()
  else {
    panic!("no command");
  };
  let format = Capability {
    key: b"object-format",
    value: Some(b"sha1"),
  };
  let lines = decode_answer(rest::BUNDLE_URI_RESPONSE, &mut room()).unwrap();
  assert_eq!(
    (
      read.command(),
      read.head,
      read.delimited,
      read.arguments,
      lines
    ),
    (
      Command::BundleUri,
      vec![Line::lf(Head::Capability(format)), Line::lf(Head::Command)],
      true,
      Arguments::BundleUri,
      vec![
        pair(b"bundle.version", b"1"),
        pair(b"bundle.mode", b"all"),
        pair(b"bundle.heuristic", b"creationToken"),
        pair(b"bundle.base.uri", b"../base.bundle"),
        pair(b"bundle.base.creationtoken", b"1"),
      ],
    )
  );
  assert_eq!(answered(&Offered(Some(LIST))), rest::BUNDLE_URI_RESPONSE);
  assert_eq!(answered(&Offered(None)), b"0000");
}

/// The messages `bytes` holds one after another, each closed by a flush.
fn by_flush(bytes: &[u8]) -> Vec<&[u8]> {
  let mut messages = Vec::new();
  let (mut start, mut at) = (0, 0);
  while let Ok(Some((packet, took))) = pktline::read(&bytes[at..], to_u64(at)) {
    at += took;
    if packet == Packet::Flush {
      messages.push(&bytes[start..at]);
      start = at;
    }
  }
  assert_eq!(at, bytes.len(), "the session ends at a flush");
  messages
}

/// Decodes and encodes again each answer of the bundle clone's session, in its order: the bytes written.
fn answers_again(answers: &[&[u8]]) -> Vec<u8> {
  let [adv, refs, bundles, fetched] = answers else {
    panic!("four answers, not {}", answers.len());
  };
  let mut out = Vec::new();
  let format = ObjectFormat::Sha1;
  advertisement::encode(&advertisement::decode(adv, &mut room()).unwrap(), &mut out).unwrap();
  ls_refs::encode_answer(
    &ls_refs::decode_answer(refs, format, &mut room()).unwrap(),
    format,
    &mut out,
  )
  .unwrap();
  encode_answer(&decode_answer(bundles, &mut room()).unwrap(), &mut out).unwrap();
  let events = fetch::decode_response(fetched, format, true, &mut room()).unwrap();
  fetch::encode_response(&events, format, true, &mut out).unwrap();
  out
}

/// A-624, golden vectors: do read the whole session of a clone stock git 2.50.1 made with `transfer.bundleURI` on,
/// message by message, and write each back. Expect: the client's `ls-refs`, `bundle-uri` and `fetch` requests, the
/// `bundle-uri` one its capabilities first; the server's advertisement and its three answers, the bundle list the same
/// as the stateless exchange's; every byte of both sides written back; and the clone's use of the bundle shown on the
/// wire: its `fetch` has the bundle's commit, `one`, which the clone filed as `refs/bundles/heads/base`.
#[test]
fn a_stock_clone_through_bundle_uri_reads_message_by_message() {
  let mut requests = Vec::new();
  let mut at = 0;
  while let Some(len) = request::whole_len(&rest::BUNDLE_CLONE_REQUEST[at..]).unwrap() {
    requests.push(request::decode(&rest::BUNDLE_CLONE_REQUEST[at..at + len], &mut room()).unwrap());
    at += len;
  }
  let mut again = Vec::new();
  for read in &requests {
    request::encode(read, &mut again).unwrap();
  }
  let commands: Vec<Command> = requests
    .iter()
    .filter_map(|read| match read {
      Request::Command(command) => Some(command.command()),
      Request::End => None,
    })
    .collect();
  let Some(Request::Command(fetch)) = requests.last() else {
    panic!("no fetch");
  };
  let Arguments::Fetch(arguments) = &fetch.arguments else {
    panic!("not a fetch");
  };
  let one = id(
    ObjectFormat::Sha1,
    "b33280297fc1fcdaa1855ec74b33387064de6a3c",
  );
  let answers = by_flush(rest::BUNDLE_CLONE_RESPONSE);
  let filed = std::str::from_utf8(rest::BUNDLE_CLONE_REFS_TXT).unwrap();
  assert_eq!(
    (
      commands,
      again,
      arguments.iter().any(|line| line.value == F::Have(one)),
      answers.get(2).copied(),
      answers_again(&answers),
      filed.contains(&format!("{one} refs/bundles/heads/base")),
    ),
    (
      vec![Command::LsRefs, Command::BundleUri, Command::Fetch],
      rest::BUNDLE_CLONE_REQUEST.to_vec(),
      true,
      Some(rest::BUNDLE_URI_RESPONSE),
      rest::BUNDLE_CLONE_RESPONSE.to_vec(),
      true,
    )
  );
}

/// The answer of one line, `text`, then a flush.
fn one_line(text: &[u8]) -> Vec<u8> {
  wire(&[Packet::Data(text), Packet::Flush])
}

/// A-624 (`bundle_uri_parse_line`, `bundle_list_update`): do decode answer lines git's client refuses, and lines whose
/// keys and values it does not know. Expect: a line with no `=`, an empty key or value, a key outside `bundle.`, a
/// version other than 1 or not in canonical decimal, a mode other than `all` and `any`, a NUL in a value, each refused
/// by kind; a bundle's key git does not know, a global key it does not know, a heuristic it does not know and a key
/// with no name after `bundle.`, each kept by name and written back.
#[test]
fn lines_git_refuses_are_refused_and_keys_it_does_not_know_are_kept() {
  for (text, refusal) in [
    (&b"bundle.version"[..], Refusal::Line { at: 0 }),
    (b"=1", Refusal::Line { at: 0 }),
    (b"bundle.mode=", Refusal::Line { at: 0 }),
    (b"core.bare=true", Refusal::Line { at: 0 }),
    (b"bundle.version=2", Refusal::Number { at: 0 }),
    (b"bundle.version=01", Refusal::Number { at: 0 }),
    (b"bundle.mode=some", Refusal::Line { at: 0 }),
    (b"bundle.base.uri=a\0b", Refusal::NotText { at: 0 }),
  ] {
    assert_eq!(
      decode_answer(&one_line(text), &mut room()),
      Err(refusal),
      "{:?}",
      text.escape_ascii().to_string()
    );
  }
  for text in [
    &b"bundle.base.flavor=vanilla"[..],
    b"bundle.lucky=yes",
    b"bundle.heuristic=lucky",
    b"bundle.=x",
    b"bundle.a.b.uri=https://example.invalid/a.b.bundle\n",
  ] {
    let bytes = one_line(text);
    let lines = decode_answer(&bytes, &mut room()).unwrap();
    let mut out = Vec::new();
    encode_answer(&lines, &mut out).unwrap();
    assert_eq!(out, bytes, "{:?}", text.escape_ascii().to_string());
  }
}

/// A-624 (`bundle_uri_command` dies on an argument): do decode a `bundle-uri` request with an argument after its
/// delimiter. Expect: refused `UnknownArgument` at the argument's line.
#[test]
fn a_bundle_uri_request_with_an_argument_is_refused() {
  let stream = wire(&[
    Packet::Data(b"command=bundle-uri\n"),
    Packet::Delim,
    Packet::Data(b"refs\n"),
    Packet::Flush,
  ]);
  let at = to_u64(4 + "command=bundle-uri\n".len() + 4);
  assert_eq!(
    request::decode(&stream, &mut room()),
    Err(Refusal::UnknownArgument { at })
  );
}

/// A-624: do answer from lists with a bundle git's client could not read back: an empty id, an id holding `=`, an empty
/// URI, a URI holding an LF; and encode a line git's client refuses. Expect: each refused by kind, nothing written.
#[test]
fn a_list_git_could_not_read_back_is_not_written() {
  let attempt = |bundle: &Bundle<'_>| {
    let offered = Offered(Some(BundleList {
      bundles: core::slice::from_ref(bundle),
      ..LIST
    }));
    let mut out = Vec::new();
    let result = answer(&offered, &mut out);
    (result, out.len())
  };
  let mode_and_version = to_u64(4 + "bundle.version=1".len() + 4 + "bundle.mode=all".len());
  let first_bundle = mode_and_version + to_u64(4 + "bundle.heuristic=creationToken".len());
  let mut out = Vec::new();
  assert_eq!(
    (
      attempt(&Bundle { id: b"", ..BASE }),
      attempt(&Bundle { id: b"a=b", ..BASE }),
      attempt(&Bundle { uri: b"", ..BASE }),
      attempt(&Bundle {
        uri: b"a\nb",
        ..BASE
      }),
      encode_answer(&[pair(b"bundle.version", b"2")], &mut out),
      out.len(),
    ),
    (
      (Err(Refusal::Line { at: first_bundle }), 0),
      (Err(Refusal::Line { at: first_bundle }), 0),
      (Err(Refusal::Line { at: first_bundle }), 0),
      (Err(Refusal::NotText { at: first_bundle }), 0),
      Err(Refusal::Number { at: 0 }),
      0,
    )
  );
}

/// Shape: the bytes a generated bundle's id takes.
const ID_BYTES: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789-.";

/// A bundle's parts a generated list holds: an id of the bytes ids use, a URI, a creation token if any.
fn any_bundle() -> impl Strategy<Value = (Vec<u8>, Vec<u8>, Option<u64>)> {
  let id_byte = (0..ID_BYTES.len()).prop_map(|at| ID_BYTES[at]);
  let uri_byte = b' '..=b'~';
  (
    proptest::collection::vec(id_byte, 1..10),
    proptest::collection::vec(uri_byte, 1..30),
    proptest::option::of(any::<u64>()),
  )
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-624: do answer any list a repository may offer, and decode the answer. Expect: `bundle.version=1`, its mode,
  /// its heuristic where it has one, then each bundle's `uri` and its `creationtoken` where it has one, in order, every
  /// line without an LF.
  #[test]
  fn any_list_is_answered_as_git_writes_its_configuration(
    any_mode in any::<bool>(),
    heuristic in any::<bool>(),
    parts in proptest::collection::vec(any_bundle(), 0..5),
  ) {
    let bundles: Vec<Bundle<'_>> = parts
      .iter()
      .map(|(id, uri, creation_token)| Bundle { id, uri, creation_token: *creation_token })
      .collect();
    let list = BundleList {
      mode: if any_mode { Mode::Any } else { Mode::All },
      heuristic: heuristic.then_some(Heuristic::CreationToken),
      bundles: &bundles,
    };
    let bytes = answered(&Offered(Some(list)));
    let read = decode_answer(&bytes, &mut Admission::new(u64::MAX)).unwrap();
    let mut expected: Vec<(Vec<u8>, Vec<u8>)> = vec![
      (b"bundle.version".to_vec(), b"1".to_vec()),
      (b"bundle.mode".to_vec(), if any_mode { b"any".to_vec() } else { b"all".to_vec() }),
    ];
    if heuristic {
      expected.push((b"bundle.heuristic".to_vec(), b"creationToken".to_vec()));
    }
    for bundle in &bundles {
      let key = |name: &str| [&b"bundle."[..], bundle.id, b".", name.as_bytes()].concat();
      expected.push((key("uri"), bundle.uri.to_vec()));
      if let Some(token) = bundle.creation_token {
        expected.push((key("creationtoken"), token.to_string().into_bytes()));
      }
    }
    let got: Vec<(Vec<u8>, Vec<u8>)> = read
      .iter()
      .map(|line| (line.value.key.to_vec(), line.value.value.to_vec()))
      .collect();
    prop_assert_eq!(
      (got, read.iter().all(|line| line.ending == Ending::NoLf)),
      (expected, true)
    );
  }
}
