use super::{Arguments, Command, CommandRequest, Head, Request, decode, encode, whole_len};
use crate::fixtures;
use crate::object_name::{HexRefusal, ObjectFormat, ObjectId};
use crate::pktline::Packet;
use crate::v2::fetch::Argument as F;
use crate::v2::ls_refs::{self, Argument as L};
use crate::v2::tests::{GOLDEN, Message, room, sha1, wire};
use crate::v2::{Capability, Ending, Line, Refusal};
use crate::{Admission, PastAdmission, to_u64};

/// The id of `main` in the golden repository.
const MAIN: &str = "d0f175d3390380c935e9b56e0cebb715995d838d";
/// The id of `side`.
const SIDE: &str = "de204ecf4cbcbf9dec27d422e67f087c136709db";
/// What the tag `v1` peels to.
const PEELED: &str = "46959f8a540a8b77be110b36029d84b840a9faff";

/// The command request `stream` holds.
fn command(stream: &[u8]) -> CommandRequest<'_> {
  match decode(stream, &mut room()).unwrap() {
    Request::Command(request) => request,
    Request::End => panic!("not a command"),
  }
}

/// The `fetch` arguments of `request`, without their endings.
fn fetch_arguments<'a>(request: &CommandRequest<'a>) -> Vec<F<'a>> {
  match &request.arguments {
    Arguments::Fetch(arguments) => arguments.iter().map(|line| line.value).collect(),
    other => panic!("not fetch: {other:?}"),
  }
}

/// `request` encoded.
fn encoded(request: &Request<'_>) -> Vec<u8> {
  let mut out = Vec::new();
  encode(request, &mut out).unwrap();
  out
}

/// A-582 part 2, golden vectors: do decode the `ls-refs` request and the six `fetch` requests sent to stock git 2.50.1,
/// then encode each. Expect: each command, its `object-format=sha1`, its delimiter and its arguments as written, every
/// line ending with an LF, and each stream's own bytes back.
#[test]
fn stock_gits_requests_decode_to_their_commands_and_arguments_and_encode_back() {
  let ls = command(fixtures::LS_REFS_REQUEST);
  assert_eq!(ls.command(), Command::LsRefs);
  assert_eq!(ls.object_format(), Some(ObjectFormat::Sha1));
  assert!(ls.delimited);
  let format = Capability {
    key: b"object-format",
    value: Some(b"sha1"),
  };
  assert_eq!(
    ls.head,
    [Line::lf(Head::Command), Line::lf(Head::Capability(format))]
  );
  let Arguments::LsRefs(arguments) = &ls.arguments else {
    panic!("not ls-refs")
  };
  let expected = [
    L::Peel,
    L::Symrefs,
    L::Unborn,
    L::RefPrefix(b"HEAD"),
    L::RefPrefix(b"refs/"),
  ];
  assert_eq!(
    arguments.iter().map(|line| line.value).collect::<Vec<_>>(),
    expected
  );
  assert!(ls_refs::holds(arguments, L::Peel) && ls_refs::holds(arguments, L::Unborn));
  assert_eq!(
    ls_refs::prefixes(arguments).collect::<Vec<_>>(),
    [&b"HEAD"[..], b"refs/"]
  );

  let (main, side, peeled) = (sha1(MAIN), sha1(SIDE), sha1(PEELED));
  let fetches: [(&[u8], Vec<F<'_>>); 6] = [
    (
      fixtures::FETCH_CLONE_REQUEST,
      vec![
        F::ThinPack,
        F::OfsDelta,
        F::NoProgress,
        F::Want(main),
        F::Done,
      ],
    ),
    (
      fixtures::FETCH_HAVES_REQUEST,
      vec![
        F::ThinPack,
        F::OfsDelta,
        F::NoProgress,
        F::Want(main),
        F::Have(side),
      ],
    ),
    (
      fixtures::FETCH_SHALLOW_REQUEST,
      vec![
        F::OfsDelta,
        F::NoProgress,
        F::Want(main),
        F::Deepen(1),
        F::Done,
      ],
    ),
    (
      fixtures::FETCH_REF_IN_WANT_REQUEST,
      vec![
        F::OfsDelta,
        F::NoProgress,
        F::WantRef(b"refs/heads/side"),
        F::Done,
      ],
    ),
    (
      fixtures::FETCH_PROGRESS_REQUEST,
      vec![F::OfsDelta, F::Want(main), F::Have(peeled), F::Done],
    ),
    (
      fixtures::FETCH_URIS_REQUEST,
      vec![
        F::OfsDelta,
        F::NoProgress,
        F::SidebandAll,
        F::Want(main),
        F::PackfileUris(b"https"),
        F::Done,
      ],
    ),
  ];
  for (stream, arguments) in fetches {
    let request = command(stream);
    assert_eq!(request.command(), Command::Fetch);
    assert_eq!(fetch_arguments(&request), arguments);
    assert_eq!(encoded(&Request::Command(request)), stream);
  }
  assert_eq!(encoded(&Request::Command(ls)), fixtures::LS_REFS_REQUEST);
}

/// A-582 part 2 (what git's own client writes, `fetch-pack.c` and `connect.c`): do decode a `fetch` written as
/// `send_fetch_request` writes one, its command, capabilities, flags and cuts without an LF and its wants, haves and
/// `done` with one, every argument kind among them; a `bundle-uri`-style head with the capabilities first; a head a
/// flush closes; and a SHA-256 request. Expect: each read as written, its endings kept, and its own bytes back.
#[test]
fn the_forms_gits_own_client_writes_decode_and_encode_back_exactly() {
  let want = format!("want {MAIN}\n");
  let have = format!("have {SIDE}\n");
  let shallow = format!("shallow {PEELED}");
  let fetch = wire(&[
    Packet::Data(b"command=fetch"),
    Packet::Data(b"agent=git/2.50.1-Darwin"),
    Packet::Data(b"session-id=abc"),
    Packet::Data(b"server-option=ci.skip"),
    Packet::Data(b"object-format=sha1"),
    Packet::Delim,
    Packet::Data(b"thin-pack"),
    Packet::Data(b"no-progress"),
    Packet::Data(b"include-tag"),
    Packet::Data(b"ofs-delta"),
    Packet::Data(b"sideband-all"),
    Packet::Data(shallow.as_bytes()),
    Packet::Data(b"deepen 2147483647"),
    Packet::Data(b"deepen-relative\n"),
    Packet::Data(b"filter blob:limit=1024"),
    Packet::Data(b"packfile-uris http,https"),
    Packet::Data(want.as_bytes()),
    Packet::Data(b"want-ref refs/heads/main\n"),
    Packet::Data(have.as_bytes()),
    Packet::Data(b"wait-for-done"),
    Packet::Data(b"done\n"),
    Packet::Flush,
  ]);
  let request = command(&fetch);
  assert_eq!(request.head[0], Line::bare(Head::Command));
  assert_eq!(request.capabilities().count(), 4);
  assert_eq!(
    fetch_arguments(&request),
    [
      F::ThinPack,
      F::NoProgress,
      F::IncludeTag,
      F::OfsDelta,
      F::SidebandAll,
      F::Shallow(sha1(PEELED)),
      F::Deepen(2_147_483_647),
      F::DeepenRelative,
      F::Filter(b"blob:limit=1024"),
      F::PackfileUris(b"http,https"),
      F::Want(sha1(MAIN)),
      F::WantRef(b"refs/heads/main"),
      F::Have(sha1(SIDE)),
      F::WaitForDone,
      F::Done,
    ]
  );
  let Arguments::Fetch(lines) = &request.arguments else {
    panic!("not fetch")
  };
  assert_eq!(lines[0].ending, Ending::NoLf);
  assert_eq!(lines[7].ending, Ending::Lf);
  assert_eq!(encoded(&Request::Command(request)), fetch);

  let cut = wire(&[
    Packet::Data(b"command=fetch"),
    Packet::Delim,
    Packet::Data(b"deepen-since 1700000000"),
    Packet::Data(b"deepen-not v1.0"),
    Packet::Data(want.as_bytes()),
    Packet::Flush,
  ]);
  let request = command(&cut);
  assert_eq!(
    fetch_arguments(&request),
    [
      F::DeepenSince(1_700_000_000),
      F::DeepenNot(b"v1.0"),
      F::Want(sha1(MAIN))
    ]
  );
  assert_eq!(request.object_format(), Some(ObjectFormat::Sha1));
  assert_eq!(encoded(&Request::Command(request)), cut);

  let capabilities_first = wire(&[
    Packet::Data(b"agent=git/2.50.1-Darwin"),
    Packet::Data(b"object-format=sha1"),
    Packet::Data(b"command=ls-refs\n"),
    Packet::Delim,
    Packet::Flush,
  ]);
  let request = command(&capabilities_first);
  assert_eq!(request.head[2], Line::lf(Head::Command));
  assert!(request.delimited && request.arguments.is_empty());
  assert_eq!(encoded(&Request::Command(request)), capabilities_first);

  let undelimited = wire(&[Packet::Data(b"command=ls-refs\n"), Packet::Flush]);
  let request = command(&undelimited);
  assert!(!request.delimited);
  assert_eq!(request.command(), Command::LsRefs);
  assert_eq!(encoded(&Request::Command(request)), undelimited);

  let id = "a".repeat(64);
  let want256 = format!("want {id}\n");
  let sha256 = wire(&[
    Packet::Data(b"command=fetch\n"),
    Packet::Data(b"object-format=sha256\n"),
    Packet::Delim,
    Packet::Data(want256.as_bytes()),
    Packet::Flush,
  ]);
  let request = command(&sha256);
  assert_eq!(request.object_format(), Some(ObjectFormat::Sha256));
  let expected = ObjectId::from_hex(ObjectFormat::Sha256, id.as_bytes()).unwrap();
  assert_eq!(fetch_arguments(&request), [F::Want(expected)]);
  assert_eq!(encoded(&Request::Command(request)), sha256);
}

/// A-582 part 2 (gitprotocol-v2: `empty-request = flush-pkt`): do decode a flush alone, and encode the end. Expect:
/// the end of the client's requests, and a flush.
#[test]
fn a_flush_alone_is_the_end_of_requests() {
  assert_eq!(decode(b"0000", &mut room()), Ok(Request::End));
  assert_eq!(encoded(&Request::End), b"0000");
  assert_eq!(
    decode(b"00000000", &mut room()),
    Err(Refusal::Trailing { at: 4 })
  );
}

/// A-624 (a server reads an id as git's server does, `get_oid_hex` taking either case; gitprotocol-pack: "both MUST
/// treat obj-id as case-insensitive"): do decode a `fetch` request whose `want` names `main` in upper case, then encode
/// it. Expect: the want read as `main`'s id, written back in lower case as git writes an id, the rest byte for byte.
#[test]
fn an_upper_case_want_is_read_and_written_back_in_lower_case() {
  let request = |want: &str| {
    wire(&[
      Packet::Data(b"command=fetch\n"),
      Packet::Delim,
      Packet::Data(want.as_bytes()),
      Packet::Data(b"done\n"),
      Packet::Flush,
    ])
  };
  let upper = request(&format!("want {}\n", MAIN.to_uppercase()));
  let read = command(&upper);
  assert_eq!(
    (fetch_arguments(&read), encoded(&Request::Command(read))),
    (
      vec![F::Want(sha1(MAIN)), F::Done],
      request(&format!("want {MAIN}\n"))
    )
  );
}

/// A-582 part 2 (`serve.c`, `ls-refs.c`, `upload-pack.c`): do decode requests out of the grammar, one fault each.
/// Expect: each refused by its kind at the line at fault: commands git does not run, a second command, none,
/// capabilities a client does not send, a bad `object-format`, arguments the command does not take, a malformed id,
/// a bad ref name, a delimiter or response end among the arguments, the bytes ending early or going on.
#[test]
fn a_request_out_of_its_grammar_is_refused_by_kind() {
  let fetch = Packet::Data(b"command=fetch\n");
  let ls = Packet::Data(b"command=ls-refs\n");
  let want = format!("want {MAIN}\n");
  let trailing = format!("want {MAIN} side\n");
  let short = format!("want {}\n", &MAIN[..39]);
  let cases = [
    (
      wire(&[Packet::Data(b"command=push\n"), Packet::Flush]),
      Refusal::UnknownCommand { at: 0 },
    ),
    (
      wire(&[Packet::Data(b"command=object-infos\n"), Packet::Flush]),
      Refusal::UnknownCommand { at: 0 },
    ),
    (
      wire(&[Packet::Data(b"command=agent\n"), Packet::Flush]),
      Refusal::UnknownCommand { at: 0 },
    ),
    (
      wire(&[Packet::Data(b"command=fetch=x\n"), Packet::Flush]),
      Refusal::UnknownCommand { at: 0 },
    ),
    (
      wire(&[Packet::Data(b"command=\n"), Packet::Flush]),
      Refusal::UnknownCommand { at: 0 },
    ),
    (
      wire(&[fetch, ls, Packet::Flush]),
      Refusal::SecondCommand { at: 18 },
    ),
    (
      wire(&[Packet::Data(b"agent=x\n"), Packet::Delim, Packet::Flush]),
      Refusal::NoCommand { at: 12 },
    ),
    (
      wire(&[Packet::Data(b"agent=x\n"), Packet::Flush]),
      Refusal::NoCommand { at: 12 },
    ),
    (
      wire(&[Packet::Delim, Packet::Flush]),
      Refusal::NoCommand { at: 0 },
    ),
    (
      wire(&[fetch, Packet::Data(b"fetch\n"), Packet::Flush]),
      Refusal::UnknownCapability { at: 18 },
    ),
    (
      wire(&[fetch, Packet::Data(b"ls-refs=x\n"), Packet::Flush]),
      Refusal::UnknownCapability { at: 18 },
    ),
    (
      wire(&[fetch, Packet::Data(b"thin-pack\n"), Packet::Flush]),
      Refusal::UnknownCapability { at: 18 },
    ),
    (
      wire(&[fetch, Packet::Data(b"object-format\n"), Packet::Flush]),
      Refusal::ObjectFormat { at: 18 },
    ),
    (
      wire(&[fetch, Packet::Data(b"object-format=md5\n"), Packet::Flush]),
      Refusal::ObjectFormat { at: 18 },
    ),
    (
      wire(&[fetch, Packet::Data(b"a gent=x\n"), Packet::Flush]),
      Refusal::Capability { at: 18 },
    ),
    (
      wire(&[ls, Packet::Delim, Packet::Data(b"peel \n"), Packet::Flush]),
      Refusal::UnknownArgument { at: 24 },
    ),
    (
      wire(&[
        ls,
        Packet::Delim,
        Packet::Data(b"ref-prefix\n"),
        Packet::Flush,
      ]),
      Refusal::UnknownArgument { at: 24 },
    ),
    (
      wire(&[ls, Packet::Delim, Packet::Data(b"done\n"), Packet::Flush]),
      Refusal::UnknownArgument { at: 24 },
    ),
    (
      wire(&[fetch, Packet::Delim, Packet::Data(b"peel\n"), Packet::Flush]),
      Refusal::UnknownArgument { at: 22 },
    ),
    (
      wire(&[fetch, Packet::Delim, Packet::Data(b"Done\n"), Packet::Flush]),
      Refusal::UnknownArgument { at: 22 },
    ),
    (
      wire(&[
        fetch,
        Packet::Delim,
        Packet::Data(b"done \n"),
        Packet::Flush,
      ]),
      Refusal::UnknownArgument { at: 22 },
    ),
    (
      wire(&[
        fetch,
        Packet::Delim,
        Packet::Data(trailing.as_bytes()),
        Packet::Flush,
      ]),
      Refusal::ObjectId {
        at: 22,
        refusal: HexRefusal::Length { len: 45 },
      },
    ),
    (
      wire(&[
        fetch,
        Packet::Delim,
        Packet::Data(short.as_bytes()),
        Packet::Flush,
      ]),
      Refusal::ObjectId {
        at: 22,
        refusal: HexRefusal::Length { len: 39 },
      },
    ),
    (
      wire(&[
        fetch,
        Packet::Delim,
        Packet::Data(b"want-ref main\n"),
        Packet::Flush,
      ]),
      Refusal::Refname { at: 22 },
    ),
    (
      wire(&[
        fetch,
        Packet::Delim,
        Packet::Data(b"want-ref refs/heads/a..b\n"),
        Packet::Flush,
      ]),
      Refusal::Refname { at: 22 },
    ),
    (
      wire(&[
        fetch,
        Packet::Delim,
        Packet::Data(want.as_bytes()),
        Packet::Delim,
        Packet::Flush,
      ]),
      Refusal::Misplaced { at: 72 },
    ),
    (
      wire(&[
        fetch,
        Packet::Delim,
        Packet::Data(want.as_bytes()),
        Packet::ResponseEnd,
      ]),
      Refusal::Misplaced { at: 72 },
    ),
    (
      wire(&[fetch, Packet::ResponseEnd]),
      Refusal::Misplaced { at: 18 },
    ),
    (
      wire(&[fetch, Packet::Delim, Packet::Data(want.as_bytes())]),
      Refusal::Unfinished { at: 72 },
    ),
    (
      wire(&[fetch, Packet::Flush, Packet::Flush]),
      Refusal::Trailing { at: 22 },
    ),
    (
      wire(&[
        fetch,
        Packet::Delim,
        Packet::Data(b"do\0ne\n"),
        Packet::Flush,
      ]),
      Refusal::NotText { at: 22 },
    ),
  ];
  for (stream, refusal) in cases {
    assert_eq!(decode(&stream, &mut room()), Err(refusal), "{stream:?}");
  }
}

/// A-582 part 2 (`upload-pack.c` `process_deepen`, `process_deepen_since`, `process_args`): do decode each value at
/// and past the range git reads it in. Expect: a depth from 1 to git's `int`'s largest read; 0, a sign, a leading zero,
/// `0x` and one past the `int` refused `Number`; a time up to one below `timestamp_t`'s largest read, that largest
/// refused; an empty filter, rev or list of protocols refused `EmptyValue`.
#[test]
fn each_argument_value_is_read_within_gits_range_and_refused_past_it() {
  let one = |text: &[u8]| {
    let stream = wire(&[
      Packet::Data(b"command=fetch\n"),
      Packet::Delim,
      Packet::Data(text),
      Packet::Flush,
    ]);
    decode(&stream, &mut room()).map(|request| match request {
      Request::Command(request) => format!("{:?}", fetch_arguments(&request).first()),
      Request::End => String::new(),
    })
  };
  let read = |argument: F<'_>| Ok(format!("{:?}", Some(argument)));
  assert_eq!(one(b"deepen 1"), read(F::Deepen(1)));
  assert_eq!(one(b"deepen 2147483647"), read(F::Deepen(2_147_483_647)));
  for refused in [
    &b"deepen 0"[..],
    b"deepen -1",
    b"deepen +1",
    b"deepen 01",
    b"deepen 0x10",
    b"deepen 2147483648",
    b"deepen ",
    b"deepen 1 ",
    b"deepen-since 18446744073709551615",
    b"deepen-since 18446744073709551616",
    b"deepen-since 007",
  ] {
    assert_eq!(one(refused), Err(Refusal::Number { at: 22 }), "{refused:?}");
  }
  assert_eq!(one(b"deepen-since 0"), read(F::DeepenSince(0)));
  assert_eq!(
    one(b"deepen-since 18446744073709551614"),
    read(F::DeepenSince(u64::MAX - 1))
  );
  for empty in [&b"filter "[..], b"deepen-not ", b"packfile-uris "] {
    assert_eq!(one(empty), Err(Refusal::EmptyValue { at: 22 }), "{empty:?}");
  }
}

/// A-582 part 2 (`upload-pack.c`: "multiple packfile-uris lines forbidden", a second filter dies; gitprotocol-v2:
/// `deepen-since` and `deepen-not` "Cannot be used with "deepen""): do decode requests breaking each rule between
/// arguments. Expect: `Repeated` at a second filter and a second list of protocols, `DeepenConflict` at the second of a
/// `deepen` and a cut whichever comes first, and a cut by time beside a cut by rev read.
#[test]
fn rules_between_arguments_are_kept() {
  let arguments = |lines: &[&[u8]]| {
    let mut packets = vec![Packet::Data(b"command=fetch\n"), Packet::Delim];
    packets.extend(lines.iter().map(|line| Packet::Data(line)));
    packets.push(Packet::Flush);
    decode(&wire(&packets), &mut room()).map(|_| ())
  };
  assert_eq!(
    arguments(&[b"filter blob:none\n", b"filter tree:0\n"]),
    Err(Refusal::Repeated { at: 43 })
  );
  assert_eq!(
    arguments(&[b"packfile-uris https\n", b"packfile-uris https\n"]),
    Err(Refusal::Repeated { at: 46 })
  );
  assert_eq!(
    arguments(&[b"deepen 1\n", b"deepen-since 5\n"]),
    Err(Refusal::DeepenConflict { at: 35 })
  );
  assert_eq!(
    arguments(&[b"deepen-not v1\n", b"deepen 1\n"]),
    Err(Refusal::DeepenConflict { at: 40 })
  );
  assert_eq!(
    arguments(&[b"deepen-since 5\n", b"deepen-not v1\n"]),
    Ok(())
  );
  assert_eq!(arguments(&[b"deepen 1\n", b"deepen 2\n"]), Ok(()));
}

/// A-582 part 2: do ask, for every golden request fed a byte at a time, whether the bytes so far hold a whole request;
/// then ask of a request with a second after it, and of bytes that are not pkt-lines. Expect: `None` until the flush's
/// last byte, then the request's length; the first request's length when a second follows; part 1's refusal.
#[test]
fn a_reader_finds_a_whole_request_as_its_flush_arrives() {
  for (name, stream, message) in GOLDEN {
    if message != Message::Request {
      continue;
    }
    for end in 0..stream.len() {
      assert_eq!(whole_len(&stream[..end]), Ok(None), "{name} at {end}");
    }
    assert_eq!(whole_len(stream), Ok(Some(stream.len())), "{name}");
    let mut two = stream.to_vec();
    two.extend_from_slice(stream);
    assert_eq!(whole_len(&two), Ok(Some(stream.len())), "{name}");
  }
  assert!(matches!(whole_len(b"00zz"), Err(Refusal::Packet(_))));
}

/// A-582 part 2: do encode requests a server must not send on, one fault each. Expect: each refused by its kind at
/// the line's place, and nothing written.
#[test]
fn a_refused_encode_writes_nothing() {
  let command = Line::lf(Head::Command);
  let fetch = |head: Vec<Line<Head<'static>>>, delimited: bool, arguments: Vec<F<'static>>| {
    Request::Command(CommandRequest {
      head,
      delimited,
      arguments: Arguments::Fetch(arguments.into_iter().map(Line::lf).collect()),
    })
  };
  let capability = |key: &'static [u8], value: Option<&'static [u8]>| {
    Line::lf(Head::Capability(Capability { key, value }))
  };
  let sha256 = ObjectId::from_hex(ObjectFormat::Sha256, "b".repeat(64).as_bytes()).unwrap();
  let cases = [
    (fetch(vec![], true, vec![]), Refusal::NoCommand { at: 0 }),
    (
      fetch(vec![command, command], true, vec![]),
      Refusal::SecondCommand { at: 18 },
    ),
    (
      fetch(vec![command, capability(b"thin-pack", None)], true, vec![]),
      Refusal::UnknownCapability { at: 18 },
    ),
    (
      fetch(
        vec![command, capability(b"object-format", Some(b"md5"))],
        true,
        vec![],
      ),
      Refusal::ObjectFormat { at: 18 },
    ),
    (
      fetch(vec![command], false, vec![F::Done]),
      Refusal::UnknownArgument { at: 18 },
    ),
    (
      fetch(vec![command], true, vec![F::Want(sha256)]),
      Refusal::ObjectId {
        at: 22,
        refusal: HexRefusal::Length { len: 64 },
      },
    ),
    (
      fetch(vec![command], true, vec![F::WantRef(b"main")]),
      Refusal::Refname { at: 22 },
    ),
    (
      fetch(vec![command], true, vec![F::Deepen(0)]),
      Refusal::Number { at: 22 },
    ),
    (
      fetch(vec![command], true, vec![F::DeepenSince(u64::MAX)]),
      Refusal::Number { at: 22 },
    ),
    (
      fetch(vec![command], true, vec![F::Filter(b"")]),
      Refusal::EmptyValue { at: 22 },
    ),
    (
      fetch(vec![command], true, vec![F::Filter(b"a\nb")]),
      Refusal::NotText { at: 22 },
    ),
    (
      fetch(
        vec![command],
        true,
        vec![F::Filter(b"blob:none"), F::Filter(b"tree:0")],
      ),
      Refusal::Repeated { at: 43 },
    ),
  ];
  for (request, refusal) in cases {
    let mut out = Vec::new();
    assert_eq!(encode(&request, &mut out), Err(refusal), "{request:?}");
    assert!(out.is_empty(), "{request:?}");
  }
}

/// A-582 part 2 (§4.2): do decode stock git's `fetch` clone request with one byte less room than its head and
/// arguments take, then with exactly the room. Expect: `PastAdmission` with nothing charged, then the request and the
/// room spent.
#[test]
fn a_decode_past_its_admission_is_refused_before_its_lists_are_made() {
  let needed =
    to_u64(2 * core::mem::size_of::<Line<Head<'_>>>() + 5 * core::mem::size_of::<Line<F<'_>>>());
  let mut short = Admission::new(needed - 1);
  assert_eq!(
    decode(fixtures::FETCH_CLONE_REQUEST, &mut short),
    Err(Refusal::PastAdmission(PastAdmission {
      asked: needed,
      left: needed - 1
    }))
  );
  assert_eq!(short.left(), needed - 1);
  let mut exact = Admission::new(needed);
  assert!(decode(fixtures::FETCH_CLONE_REQUEST, &mut exact).is_ok());
  assert_eq!(exact.left(), 0);
}
