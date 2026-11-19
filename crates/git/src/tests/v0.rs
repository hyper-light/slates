use proptest::prelude::*;

use super::{
  AckStatus, AdvertisedRef, Advertisement, Capabilities, Capability, ClientLine, LineRefusal,
  Refusal, ServerLine, ShallowUpdate, Version, WriteRefusal,
};
use crate::object_name::{HexRefusal, ObjectFormat, ObjectId};
use crate::pktline::{self, MAX_DATA, Packet};
use crate::sideband::{self, Band};
use crate::{Admission, PastAdmission, to_u64};

/// The golden vectors' SHA-256 sums as `fixtures/v0/make-v0-fixtures.sh` wrote them, a line a file.
const SUMS: &str = include_str!("../../fixtures/v0/SHA256SUMS");

/// A golden vector by its name in `fixtures/v0`, compiled in.
macro_rules! fixture {
  ($name:literal) => {
    (
      $name,
      include_bytes!(concat!("../../fixtures/v0/", $name)).as_slice(),
    )
  };
}

/// Every file the script wrote, by its name in `SHA256SUMS`.
const FILES: [(&str, &[u8]); 39] = [
  fixture!("advertisement-empty.bin"),
  fixture!("advertisement-receive-pack.bin"),
  fixture!("advertisement-sha256.bin"),
  fixture!("advertisement-shallow.bin"),
  fixture!("advertisement-v0.bin"),
  fixture!("advertisement-v1.bin"),
  fixture!("clone-v0.request.bin"),
  fixture!("clone-v0.response.bin"),
  fixture!("clone-v1.request.bin"),
  fixture!("clone-v1.response.bin"),
  fixture!("deepen-v0.request.bin"),
  fixture!("deepen-v0.response.bin"),
  fixture!("depth-v0.request.bin"),
  fixture!("depth-v0.response.bin"),
  fixture!("detailed-done.request.bin"),
  fixture!("detailed-done.response.bin"),
  fixture!("exclude-v0.request.bin"),
  fixture!("exclude-v0.response.bin"),
  fixture!("fetch-v0.request.bin"),
  fixture!("fetch-v0.response.bin"),
  fixture!("fetch-v1.request.bin"),
  fixture!("fetch-v1.response.bin"),
  fixture!("filter-v0.request.bin"),
  fixture!("filter-v0.response.bin"),
  fixture!("git-version.txt"),
  fixture!("log.txt"),
  fixture!("multi-ack.request.bin"),
  fixture!("multi-ack.response.bin"),
  fixture!("plain.request.bin"),
  fixture!("plain.response.bin"),
  fixture!("ready.request.bin"),
  fixture!("ready.response.bin"),
  fixture!("refs.txt"),
  fixture!("since-v0.request.bin"),
  fixture!("since-v0.response.bin"),
  fixture!("unknown-want.request.bin"),
  fixture!("unknown-want.response.bin"),
  fixture!("unshallow.request.bin"),
  fixture!("unshallow.response.bin"),
];

/// The advertisements `upload-pack` and `receive-pack` wrote alone, each a whole stream.
const ADVERTISEMENTS: [&str; 6] = [
  "advertisement-v0.bin",
  "advertisement-v1.bin",
  "advertisement-empty.bin",
  "advertisement-shallow.bin",
  "advertisement-sha256.bin",
  "advertisement-receive-pack.bin",
];

/// Each exchange the script captured, and whether its answer opens with the advertisement: the exchanges a tee copied
/// do; the stateless ones answer their request alone.
const EXCHANGES: [(&str, bool); 15] = [
  ("clone-v0", true),
  ("clone-v1", true),
  ("fetch-v0", true),
  ("fetch-v1", true),
  ("depth-v0", true),
  ("deepen-v0", true),
  ("since-v0", true),
  ("exclude-v0", true),
  ("filter-v0", true),
  ("multi-ack", false),
  ("plain", false),
  ("detailed-done", false),
  ("ready", false),
  ("unshallow", false),
  ("unknown-want", false),
];

/// The id the script's `printf 'not in the server\n' | git hash-object --stdin` made: an object the server never had.
const UNKNOWN: &str = "86e52f206da93ad5319625df790d3a5ed9f57f60";

/// The SHA-256 repository's one commit, as its advertisement names it.
const SHA256_TIP: &str = "72c9f67255c8696e8ab06b03065d9fffbf200461a1ef966ee954cb1de52d5331";

/// The golden vector named `name`.
fn file(name: &str) -> &'static [u8] {
  FILES
    .iter()
    .find(|(known, _)| *known == name)
    .map(|(_, bytes)| *bytes)
    .unwrap_or_else(|| panic!("no golden vector {name}"))
}

/// The SHA-1 name `hex` writes.
fn sha1(hex: &str) -> ObjectId {
  ObjectId::from_hex(ObjectFormat::Sha1, hex.as_bytes()).unwrap()
}

/// The commit `log.txt` recorded with the subject `subject`.
fn commit(subject: &str) -> ObjectId {
  let log = std::str::from_utf8(file("log.txt")).unwrap();
  let line = log
    .lines()
    .find(|line| line.split_once(' ').unwrap().1 == subject)
    .unwrap_or_else(|| panic!("no commit {subject}"));
  sha1(line.split_once(' ').unwrap().0)
}

/// The refs `refs.txt` recorded, by name.
fn recorded_refs() -> Vec<(&'static [u8], ObjectId)> {
  std::str::from_utf8(file("refs.txt"))
    .unwrap()
    .lines()
    .map(|line| {
      let (id, name) = line.split_once(' ').unwrap();
      (name.as_bytes(), sha1(id))
    })
    .collect()
}

/// The ref `refs.txt` recorded as `name`.
fn recorded(name: &str) -> ObjectId {
  recorded_refs()
    .into_iter()
    .find(|(known, _)| *known == name.as_bytes())
    .unwrap()
    .1
}

/// Room for any message these tests decode.
fn room() -> Admission {
  Admission::new(u64::MAX)
}

/// The whole advertisement `bytes` begin with.
fn decoded(bytes: &[u8]) -> (Advertisement<'_>, usize) {
  Advertisement::decode(bytes, &mut room())
    .unwrap()
    .expect("a whole advertisement")
}

/// What `write` writes, which must not be refused.
fn written(write: impl FnOnce(&mut Vec<u8>) -> Result<(), WriteRefusal>) -> Vec<u8> {
  let mut out = Vec::new();
  write(&mut out).unwrap();
  out
}

/// `data` as one pkt-line.
fn packet(data: &[u8]) -> Vec<u8> {
  written(|out| {
    pktline::write(Packet::Data(data), out).unwrap();
    Ok(())
  })
}

/// A stream of `lines`, each a data packet, then a flush.
fn stream(lines: &[&[u8]]) -> Vec<u8> {
  let mut bytes: Vec<u8> = lines.iter().flat_map(|line| packet(line)).collect();
  bytes.extend_from_slice(b"0000");
  bytes
}

/// The data of the one packet `bytes` hold.
fn data_of(bytes: &[u8]) -> &[u8] {
  match pktline::read(bytes, 0) {
    Ok(Some((Packet::Data(data), took))) if took == bytes.len() => data,
    other => panic!("not one data packet: {other:?}"),
  }
}

/// A line without its one trailing LF.
fn chomped(line: &[u8]) -> &[u8] {
  line.strip_suffix(b"\n").unwrap_or(line)
}

/// The packets of a whole stream.
fn packets(bytes: &[u8]) -> Vec<Packet<'_>> {
  pktline::decode(bytes, &mut room()).unwrap()
}

/// What a server answered in one exchange.
struct Answer<'a> {
  advertisement: Option<Advertisement<'a>>,
  shallow_update: Option<ShallowUpdate>,
  lines: Vec<ServerLine<'a>>,
  pack: Vec<u8>,
}

/// Reads a request line by line (`None` for a flush), checking each line writes back to its packet's bytes.
fn read_request<'a>(name: &str, request: &'a [u8]) -> Vec<Option<ClientLine<'a>>> {
  let mut read = Vec::new();
  let mut again = Vec::new();
  for packet in packets(request) {
    match packet {
      Packet::Data(data) => {
        let line = ClientLine::read(data, ObjectFormat::Sha1)
          .unwrap_or_else(|refusal| panic!("{name}: {refusal:?} in {:?}", data.escape_ascii()));
        line.write(&mut again).unwrap();
        read.push(Some(line));
      }
      Packet::Flush => {
        pktline::write(Packet::Flush, &mut again).unwrap();
        read.push(None);
      }
      other => panic!("{name}: {other:?} in a request"),
    }
  }
  assert_eq!(again, request, "{name}: the request written back");
  read
}

/// Reads an answer: the advertisement when it opens with one, the shallow-update when the request asked for depth, the
/// server's lines, then the pack on side-band band 1 up to its flush. Every message and line is checked to write back to
/// its bytes.
fn read_answer<'a>(
  name: &str,
  request: &[Option<ClientLine<'_>>],
  response: &'a [u8],
  advertised: bool,
) -> Answer<'a> {
  let mut at = 0;
  let mut advertisement = None;
  if advertised {
    let (read, took) = decoded(response);
    assert_eq!(written(|out| read.write(out)), &response[..took], "{name}");
    advertisement = Some(read);
    at = took;
  }
  let asked_depth = request.iter().flatten().any(|line| {
    matches!(
      line,
      ClientLine::Deepen(_) | ClientLine::DeepenSince(_) | ClientLine::DeepenNot(_)
    )
  });
  let mut shallow_update = None;
  if asked_depth {
    let (update, took) = ShallowUpdate::decode(&response[at..], ObjectFormat::Sha1, &mut room())
      .unwrap()
      .expect("a whole shallow-update");
    assert_eq!(
      written(|out| update.write(out)),
      &response[at..at + took],
      "{name}"
    );
    shallow_update = Some(update);
    at += took;
  }
  let (lines, pack) = read_negotiation(name, &response[at..]);
  Answer {
    advertisement,
    shallow_update,
    lines,
    pack,
  }
}

/// Reads the server's lines until the pack, then the pack's band-1 bytes up to its flush.
fn read_negotiation<'a>(name: &str, rest: &'a [u8]) -> (Vec<ServerLine<'a>>, Vec<u8>) {
  let mut lines = Vec::new();
  let mut pack = Vec::new();
  let mut in_pack = false;
  let mut ended = false;
  for packet in packets(rest) {
    assert!(!ended, "{name}: a packet after the pack's flush");
    match packet {
      Packet::Data(data) if !in_pack && !matches!(data.first(), Some(1..=3)) => {
        let line = ServerLine::read(data, ObjectFormat::Sha1)
          .unwrap_or_else(|refusal| panic!("{name}: {refusal:?}"));
        assert_eq!(written(|out| line.write(out)), packet(data), "{name}");
        lines.push(line);
      }
      Packet::Data(data) => {
        in_pack = true;
        let (band, bytes) = sideband::demux(data).unwrap();
        if band == Band::Data {
          pack.extend_from_slice(bytes);
        }
      }
      Packet::Flush if in_pack => ended = true,
      other => panic!("{name}: {other:?} outside the pack"),
    }
  }
  (lines, pack)
}

/// A first `want`'s capabilities, by name.
fn want_capabilities<'a>(line: &Option<ClientLine<'a>>) -> Vec<&'a [u8]> {
  match line {
    Some(ClientLine::Want {
      capabilities: Some(capabilities),
      ..
    }) => capabilities
      .iter()
      .map(|capability| capability.name)
      .collect(),
    other => panic!("not a first want: {other:?}"),
  }
}

/// A `want` with no capabilities.
fn want(id: ObjectId) -> Option<ClientLine<'static>> {
  Some(ClientLine::Want {
    id,
    capabilities: None,
  })
}

/// An `ACK`.
fn ack(id: ObjectId, status: AckStatus) -> ServerLine<'static> {
  ServerLine::Ack { id, status }
}

/// Format: a pack's first eight bytes, its signature and version 2 (gitformat-pack).
const PACK_HEAD: &[u8] = b"PACK\0\0\0\x02";

/// A-604, golden vectors: do hash every file `make-v0-fixtures.sh` wrote with SHA-256 (aws-lc-rs) and compare each
/// with the sum the script recorded. Expect: every sum equal and every file listed, so the bytes these tests read are
/// the ones stock git 2.50.1 wrote.
#[test]
fn every_golden_vector_is_the_file_the_script_recorded() {
  let mut listed = Vec::new();
  for line in SUMS.lines() {
    let (sum, name) = line.split_once("  ").expect("a sum and a name");
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, file(name));
    let hex: String = digest
      .as_ref()
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect();
    assert_eq!(hex, sum, "{name}");
    listed.push(name);
  }
  let mut names: Vec<&str> = FILES.iter().map(|(name, _)| *name).collect();
  names.sort_unstable();
  assert_eq!(listed, names);
  assert!(file("git-version.txt").starts_with(b"git version 2.50.1"));
}

/// A-604, golden vectors: do decode the advertisement stock git's `upload-pack` wrote under protocol v0 and under v1,
/// and write each back. Expect: v1's opens with `version 1` and is v0's after it; `HEAD` first, then every ref
/// `refs.txt` recorded with the object it names, the annotated tag `v1` with its peeled commit (`two`, `main~1`); the
/// format SHA-1, `symref=HEAD:refs/heads/main` and git's agent among the capabilities; the whole stream taken; every
/// byte written back.
#[test]
fn upload_packs_advertisement_under_v0_and_v1_reads_as_the_refs_recorded_and_writes_back() {
  for (name, version) in [
    ("advertisement-v0.bin", Version::V0),
    ("advertisement-v1.bin", Version::V1),
  ] {
    let bytes = file(name);
    let (advertisement, took) = decoded(bytes);
    assert_eq!(took, bytes.len(), "{name}");
    assert_eq!(advertisement.version, version, "{name}");
    let mut expected = vec![AdvertisedRef {
      id: recorded("refs/heads/main"),
      name: b"HEAD",
      peeled: None,
    }];
    for (refname, id) in recorded_refs() {
      expected.push(AdvertisedRef {
        id,
        name: refname,
        peeled: (refname == b"refs/tags/v1").then(|| commit("two")),
      });
    }
    assert_eq!(advertisement.refs, expected, "{name}");
    assert!(advertisement.shallows.is_empty());
    let capabilities = advertisement.capabilities;
    assert_eq!(capabilities.object_format(), Ok(ObjectFormat::Sha1));
    assert_eq!(
      capabilities.get(b"symref"),
      Some(Capability {
        name: b"symref",
        value: Some(b"HEAD:refs/heads/main"),
      })
    );
    let agent = capabilities.get(b"agent").unwrap().value.unwrap();
    assert!(
      agent.starts_with(b"git/2.50.1"),
      "{:?}",
      agent.escape_ascii()
    );
    for known in [
      &b"multi_ack"[..],
      b"multi_ack_detailed",
      b"side-band-64k",
      b"shallow",
      b"deepen-since",
      b"no-done",
      b"filter",
    ] {
      assert!(capabilities.get(known).is_some(), "{name}: {known:?}");
    }
    assert_eq!(written(|out| advertisement.write(out)), bytes, "{name}");
  }
}

/// A-604, golden vectors: do decode the advertisement of an empty repository, of a shallow one, of a SHA-256 one and
/// `receive-pack`'s, and write each back. Expect: the empty one has no refs, its `capabilities^{}` line carrying the
/// capabilities; the shallow one names its shallow commit (`two`); the SHA-256 one reads its 64-digit names in the
/// format its `object-format=sha256` names; `receive-pack`'s carries its own capabilities on its first ref,
/// `refs/heads/main`, and peels nothing; every byte written back.
#[test]
fn the_empty_shallow_sha256_and_receive_pack_advertisements_read_and_write_back() {
  let empty = file("advertisement-empty.bin");
  let (advertisement, _) = decoded(empty);
  assert!(advertisement.refs.is_empty());
  assert!(advertisement.capabilities.get(b"multi_ack").is_some());
  assert!(advertisement.capabilities.get(b"symref").is_none());
  assert!(empty[4..].starts_with(b"0000000000000000000000000000000000000000 capabilities^{}\0"));
  assert_eq!(written(|out| advertisement.write(out)), empty);

  let shallow = file("advertisement-shallow.bin");
  let (advertisement, _) = decoded(shallow);
  assert_eq!(advertisement.shallows, [commit("two")]);
  assert_eq!(advertisement.refs[0].name, b"HEAD");
  assert_eq!(written(|out| advertisement.write(out)), shallow);

  let sha256 = file("advertisement-sha256.bin");
  let (advertisement, _) = decoded(sha256);
  assert_eq!(
    advertisement.capabilities.object_format(),
    Ok(ObjectFormat::Sha256)
  );
  let tip = ObjectId::from_hex(ObjectFormat::Sha256, SHA256_TIP.as_bytes()).unwrap();
  let names: Vec<(&[u8], ObjectId)> = advertisement
    .refs
    .iter()
    .map(|reference| (reference.name, reference.id))
    .collect();
  let expected: [(&[u8], ObjectId); 2] = [(b"HEAD", tip), (b"refs/heads/main", tip)];
  assert_eq!(names, expected);
  assert_eq!(written(|out| advertisement.write(out)), sha256);

  let receive = file("advertisement-receive-pack.bin");
  let (advertisement, _) = decoded(receive);
  let expected: Vec<AdvertisedRef<'_>> = recorded_refs()
    .into_iter()
    .map(|(name, id)| AdvertisedRef {
      id,
      name,
      peeled: None,
    })
    .collect();
  assert_eq!(advertisement.refs, expected);
  for known in [
    &b"report-status"[..],
    b"report-status-v2",
    b"delete-refs",
    b"atomic",
    b"side-band-64k",
  ] {
    assert!(advertisement.capabilities.get(known).is_some(), "{known:?}");
  }
  assert_eq!(written(|out| advertisement.write(out)), receive);
}

/// A-604, golden vectors: do read every exchange stock `git fetch-pack` and `git upload-pack` had, and every stateless
/// request and answer, line by line, writing each line and message back. Expect: every byte of every request and
/// answer read as a line of its kind and written back; v0's and v1's requests alike; the answers as git's negotiation
/// rules give them (a clone's `NAK`; a fetch's `ACK common` then its last `ACK`; `multi_ack`'s `continue`s and `NAK`;
/// `ready`; the plain mode's `ACK`s and no `NAK`); each depth request answered by its shallow-update; the unknown want
/// refused by `ERR`; and each pack whole on band 1.
#[test]
fn every_exchange_reads_line_by_line_and_writes_back_to_its_bytes() {
  for (name, advertised) in EXCHANGES {
    let request = read_request(name, file(&format!("{name}.request.bin")));
    let answer = read_answer(
      name,
      &request,
      file(&format!("{name}.response.bin")),
      advertised,
    );
    assert_eq!(answer.advertisement.is_some(), advertised);
    if answer
      .lines
      .iter()
      .all(|line| !matches!(line, ServerLine::Error(_)))
      && request.contains(&Some(ClientLine::Done))
    {
      assert!(answer.pack.starts_with(PACK_HEAD), "{name}");
    }
    check_exchange(name, &request, &answer);
  }
}

/// The lines each exchange must hold, by its name.
fn check_exchange(name: &str, request: &[Option<ClientLine<'_>>], answer: &Answer<'_>) {
  let (tip, side, tag) = (
    recorded("refs/heads/main"),
    recorded("refs/heads/side"),
    recorded("refs/tags/v1"),
  );
  let (two, one) = (commit("two"), commit("one"));
  let unknown = sha1(UNKNOWN);
  let none = ShallowUpdate::default();
  match name {
    "clone-v0" | "clone-v1" => {
      assert_eq!(
        request[1..],
        [
          want(tip),
          want(side),
          want(tag),
          None,
          Some(ClientLine::Done)
        ]
      );
      assert_eq!(answer.lines, [ServerLine::Nak]);
    }
    "fetch-v0" | "fetch-v1" => {
      assert_eq!(
        request[1..],
        [None, Some(ClientLine::Have(two)), Some(ClientLine::Done)]
      );
      assert_eq!(
        answer.lines,
        [ack(two, AckStatus::Common), ack(two, AckStatus::Plain)]
      );
    }
    "depth-v0" => {
      assert_eq!(request[2], Some(ClientLine::Deepen(1)));
      let update = answer.shallow_update.as_ref().unwrap();
      assert_eq!(update.shallow, [tip]);
      assert!(update.unshallow.is_empty());
    }
    "deepen-v0" => {
      assert!(want_capabilities(&request[0]).contains(&&b"deepen-relative"[..]));
      assert_eq!(
        request[1..],
        [
          Some(ClientLine::Shallow(tip)),
          Some(ClientLine::Deepen(1)),
          None,
          Some(ClientLine::Have(tip)),
          Some(ClientLine::Done)
        ]
      );
      let update = answer.shallow_update.as_ref().unwrap();
      assert_eq!(
        (&update.shallow, &update.unshallow),
        (&vec![two], &vec![tip])
      );
      assert_eq!(
        answer.lines,
        [ack(tip, AckStatus::Common), ack(tip, AckStatus::Plain)]
      );
    }
    "since-v0" => {
      assert_eq!(request[2], Some(ClientLine::DeepenSince(1_112_911_993)));
      assert_eq!(answer.shallow_update.as_ref(), Some(&none));
    }
    "exclude-v0" => {
      assert_eq!(request[2], Some(ClientLine::DeepenNot(b"side")));
      assert_eq!(answer.shallow_update.as_ref().unwrap().shallow, [tip]);
    }
    "filter-v0" => {
      assert!(want_capabilities(&request[0]).contains(&&b"filter"[..]));
      assert_eq!(request[4], Some(ClientLine::Filter(b"blob:none")));
      assert_eq!(answer.lines, [ServerLine::Nak]);
    }
    "multi-ack" => {
      assert_eq!(want_capabilities(&request[0])[0], b"multi_ack");
      assert_eq!(
        answer.lines,
        [
          ack(two, AckStatus::Continue),
          ack(one, AckStatus::Continue),
          ServerLine::Nak
        ]
      );
    }
    "plain" => assert_eq!(
      answer.lines,
      [ack(two, AckStatus::Plain), ack(one, AckStatus::Plain)]
    ),
    "detailed-done" => assert_eq!(
      answer.lines,
      [ack(two, AckStatus::Common), ack(two, AckStatus::Plain)]
    ),
    "ready" => assert_eq!(
      answer.lines,
      [
        ack(two, AckStatus::Common),
        ack(two, AckStatus::Ready),
        ServerLine::Nak
      ]
    ),
    "unshallow" => {
      assert_eq!(
        request[1..3],
        [Some(ClientLine::Shallow(tip)), Some(ClientLine::Deepen(2))]
      );
      let update = answer.shallow_update.as_ref().unwrap();
      assert_eq!(
        (&update.shallow, &update.unshallow),
        (&vec![two], &vec![tip])
      );
      assert_eq!(answer.lines, [ServerLine::Nak]);
    }
    "unknown-want" => {
      assert_eq!(
        request[0],
        Some(ClientLine::Want {
          id: unknown,
          capabilities: Capabilities::read(b"side-band-64k ofs-delta").ok(),
        })
      );
      let message = format!("upload-pack: not our ref {UNKNOWN}");
      assert_eq!(answer.lines, [ServerLine::Error(message.as_bytes())]);
    }
    other => panic!("no expectations for {other}"),
  }
}

/// gitprotocol-pack's own examples ("Reference Discovery" and "Packfile Negotiation"), each line as its length counts
/// it: the document breaks the long lines for the page, and the lengths count each as one line ended by LF.
const DOCUMENT_ADVERTISEMENT: &[u8] = b"000eversion 1\n\
00887217a7c7e582c46cec22a130adf4b9d7d950fba0 HEAD\0multi_ack thin-pack side-band side-band-64k ofs-delta shallow no-progress include-tag\n\
00441d3fcd5ced445d1abc402225c0b8a1299641f497 refs/heads/integration\n\
003f7217a7c7e582c46cec22a130adf4b9d7d950fba0 refs/heads/master\n\
003cb88d2441cac0977faf98efc80305012112238d9d refs/tags/v0.9\n\
003c525128480b96c89e6418b1e40909bf6c5b2d580f refs/tags/v1.0\n\
003fe92df48743b7bc7d26bcaabfddde0a1e20cae47c refs/tags/v1.0^{}\n\
0000";

/// The document's clone and fetch requests.
const DOCUMENT_REQUESTS: &[u8] =
  b"0054want 74730d410fcb6603ace96f1dc55ea6196122532d multi_ack side-band-64k ofs-delta\n\
0032want 7d1665144a3a975c05f1f43902ddaf084e784dbe\n\
0032want 5a3f6be755bbb7deae50065988cbfa1ffa9ab68a\n\
0032want 7e47fe2bd8d01d481f44d7af0531bd93d3b21c01\n\
0032want 74730d410fcb6603ace96f1dc55ea6196122532d\n\
0000\
0009done\n\
0032have 7e47fe2bd8d01d481f44d7af0531bd93d3b21c01\n\
0032have 74730d410fcb6603ace96f1dc55ea6196122532d\n\
0000";

/// The document's answers: `NAK`; then `ACK continue` twice, `NAK` and the last `ACK`.
const DOCUMENT_ANSWERS: &[u8] = b"0008NAK\n\
003aACK 7e47fe2bd8d01d481f44d7af0531bd93d3b21c01 continue\n\
003aACK 74730d410fcb6603ace96f1dc55ea6196122532d continue\n\
0008NAK\n\
0031ACK 74730d410fcb6603ace96f1dc55ea6196122532d\n";

/// A-604: do read gitprotocol-pack's own examples, its v1 reference discovery, its clone and fetch requests and its
/// server's answers, and write each back. Expect: the advertisement v1, `HEAD` first, `v0.9` unpeeled and `v1.0` with
/// its peeled line folded in, eight capabilities and SHA-1 by default; every request and answer line read and written
/// back to the document's bytes.
#[test]
fn the_documents_own_examples_read_and_write_as_written() {
  let (advertisement, took) = decoded(DOCUMENT_ADVERTISEMENT);
  assert_eq!(took, DOCUMENT_ADVERTISEMENT.len());
  assert_eq!(advertisement.version, Version::V1);
  let names: Vec<&[u8]> = advertisement
    .refs
    .iter()
    .map(|reference| reference.name)
    .collect();
  assert_eq!(
    names,
    [
      &b"HEAD"[..],
      b"refs/heads/integration",
      b"refs/heads/master",
      b"refs/tags/v0.9",
      b"refs/tags/v1.0"
    ]
  );
  assert_eq!(advertisement.refs[3].peeled, None);
  assert_eq!(
    advertisement.refs[4].peeled,
    Some(sha1("e92df48743b7bc7d26bcaabfddde0a1e20cae47c"))
  );
  assert_eq!(advertisement.capabilities.iter().count(), 8);
  assert_eq!(
    advertisement.capabilities.object_format(),
    Ok(ObjectFormat::Sha1)
  );
  assert_eq!(
    written(|out| advertisement.write(out)),
    DOCUMENT_ADVERTISEMENT
  );
  let request = read_request("the document's requests", DOCUMENT_REQUESTS);
  assert_eq!(request.len(), 10);
  let (lines, pack) = read_negotiation("the document's answers", DOCUMENT_ANSWERS);
  assert_eq!(lines.len(), 5);
  assert!(pack.is_empty());
}

/// A-604: do read an advertisement and a first `want` whose capabilities include names git 2.50.1 does not define, one
/// with a value holding `=` and bytes past ASCII, then write the advertisement again from a list the caller made.
/// Expect: each capability kept by its name, in its place, with its value; nothing refused; the object format still the
/// one `object-format` names; written back to the same bytes from the read list and from the made one.
#[test]
fn a_capability_git_does_not_know_is_kept_by_name_and_written_back() {
  let tip = recorded("refs/heads/main");
  let first =
    format!("{tip} HEAD\0multi_ack frobnicate future-thing=a=b/c\u{e9} object-format=sha1 x_y-2\n");
  let bytes = stream(&[first.as_bytes()]);
  let (advertisement, _) = decoded(&bytes);
  let names: Vec<&[u8]> = advertisement
    .capabilities
    .iter()
    .map(|capability| capability.name)
    .collect();
  assert_eq!(
    names,
    [
      &b"multi_ack"[..],
      b"frobnicate",
      b"future-thing",
      b"object-format",
      b"x_y-2"
    ]
  );
  assert_eq!(
    advertisement
      .capabilities
      .get(b"future-thing")
      .unwrap()
      .value,
    Some("a=b/c\u{e9}".as_bytes())
  );
  assert_eq!(
    advertisement.capabilities.object_format(),
    Ok(ObjectFormat::Sha1)
  );
  assert_eq!(written(|out| advertisement.write(out)), bytes);

  let made = [
    Capability {
      name: b"multi_ack",
      value: None,
    },
    Capability {
      name: b"frobnicate",
      value: None,
    },
    Capability {
      name: b"future-thing",
      value: Some("a=b/c\u{e9}".as_bytes()),
    },
    Capability {
      name: b"object-format",
      value: Some(b"sha1"),
    },
    Capability {
      name: b"x_y-2",
      value: None,
    },
  ];
  let remade = Advertisement {
    capabilities: Capabilities::listed(&made),
    ..advertisement.clone()
  };
  assert_eq!(remade, advertisement);
  assert_eq!(written(|out| remade.write(out)), bytes);

  let want = format!("want {tip} multi_ack_detailed teleport=yes side-band-64k\n");
  let line = ClientLine::read(want.as_bytes(), ObjectFormat::Sha1).unwrap();
  assert_eq!(
    want_capabilities(&Some(line)),
    [&b"multi_ack_detailed"[..], b"teleport", b"side-band-64k"]
  );
  assert_eq!(written(|out| line.write(out)), packet(want.as_bytes()));
}

/// A-604: do read each line with and without the LF git's writer ends it with, and its object name in upper case.
/// Expect: each read the same either way, as git's readers read it, and written back as git writes it: `want`, `have`
/// and `done` with LF; the request's `shallow` and `deepen` without.
#[test]
fn a_line_reads_with_or_without_its_lf_and_its_name_in_either_case() {
  let tip = recorded("refs/heads/main");
  let upper = tip.to_string().to_ascii_uppercase();
  for (as_git_writes, other) in [
    (format!("want {tip}\n"), format!("want {upper}")),
    (format!("have {tip}\n"), format!("have {upper}")),
    ("done\n".to_owned(), "done".to_owned()),
    (format!("shallow {tip}"), format!("shallow {upper}\n")),
    (
      "deepen 2147483647".to_owned(),
      "deepen 2147483647\n".to_owned(),
    ),
    ("deepen-since 0".to_owned(), "deepen-since 0\n".to_owned()),
  ] {
    let line = ClientLine::read(as_git_writes.as_bytes(), ObjectFormat::Sha1).unwrap();
    assert_eq!(
      ClientLine::read(other.as_bytes(), ObjectFormat::Sha1),
      Ok(line)
    );
    assert_eq!(
      written(|out| line.write(out)),
      packet(as_git_writes.as_bytes())
    );
  }
  for (as_git_writes, other) in [
    ("NAK\n".to_owned(), "NAK".to_owned()),
    (format!("ACK {tip} ready\n"), format!("ACK {upper} ready")),
  ] {
    let line = ServerLine::read(as_git_writes.as_bytes(), ObjectFormat::Sha1).unwrap();
    assert_eq!(
      ServerLine::read(other.as_bytes(), ObjectFormat::Sha1),
      Ok(line)
    );
    assert_eq!(
      written(|out| line.write(out)),
      packet(as_git_writes.as_bytes())
    );
  }
}

/// A-604, refused by kind: do read client lines each malformed one way. Expect: each refused with its kind and the
/// byte where it stopped, none read as another line.
#[test]
fn malformed_client_lines_are_refused_by_kind_where_they_stop() {
  let tip = recorded("refs/heads/main").to_string();
  let short = &tip[..39];
  let cases = [
    (
      format!("want {short}"),
      LineRefusal::Id {
        at: 5,
        refusal: HexRefusal::Length { len: 39 },
      },
    ),
    (
      format!("want {short}g"),
      LineRefusal::Id {
        at: 5,
        refusal: HexRefusal::NotHex { at: 39 },
      },
    ),
    (
      format!("want {tip}\0"),
      LineRefusal::Id {
        at: 5,
        refusal: HexRefusal::Length { len: 41 },
      },
    ),
    (
      format!("want {tip}  multi_ack"),
      LineRefusal::Capability { at: 46 },
    ),
    (
      format!("want {tip} multi_ack "),
      LineRefusal::Capability { at: 56 },
    ),
    (
      format!("want {tip} Multi_ack"),
      LineRefusal::Capability { at: 46 },
    ),
    (
      format!("want {tip} agent=a\tb"),
      LineRefusal::Capability { at: 53 },
    ),
    (format!("want {tip} =x"), LineRefusal::Capability { at: 46 }),
    (format!("have {tip} x"), LineRefusal::Form { at: 45 }),
    ("have".to_owned(), LineRefusal::Form { at: 4 }),
    (
      "have ".to_owned(),
      LineRefusal::Id {
        at: 5,
        refusal: HexRefusal::Length { len: 0 },
      },
    ),
    (
      format!("shallow {tip}x"),
      LineRefusal::Id {
        at: 8,
        refusal: HexRefusal::Length { len: 41 },
      },
    ),
    (format!("wants {tip}"), LineRefusal::Unknown),
    ("done ".to_owned(), LineRefusal::Form { at: 4 }),
    ("done\r".to_owned(), LineRefusal::Unknown),
    ("".to_owned(), LineRefusal::Unknown),
    ("deepen 0".to_owned(), LineRefusal::Number { at: 7 }),
    ("deepen 01".to_owned(), LineRefusal::Number { at: 7 }),
    ("deepen -1".to_owned(), LineRefusal::Number { at: 7 }),
    ("deepen +1".to_owned(), LineRefusal::Number { at: 7 }),
    ("deepen 0x10".to_owned(), LineRefusal::Number { at: 7 }),
    (
      "deepen 2147483648".to_owned(),
      LineRefusal::Number { at: 7 },
    ),
    ("deepen ".to_owned(), LineRefusal::Number { at: 7 }),
    (
      "deepen-since 18446744073709551615".to_owned(),
      LineRefusal::Number { at: 13 },
    ),
    (
      "deepen-since 18446744073709551616".to_owned(),
      LineRefusal::Number { at: 13 },
    ),
    ("deepen-not ".to_owned(), LineRefusal::Name { at: 11 }),
    ("deepen-not a b".to_owned(), LineRefusal::Name { at: 12 }),
    ("deepen-not a\x7f".to_owned(), LineRefusal::Name { at: 12 }),
    ("filter ".to_owned(), LineRefusal::Text { at: 7 }),
    ("filter a\0b".to_owned(), LineRefusal::Text { at: 8 }),
    ("filter a\nb".to_owned(), LineRefusal::Text { at: 8 }),
    ("ERR".to_owned(), LineRefusal::Form { at: 3 }),
    ("ERR a\0".to_owned(), LineRefusal::Text { at: 5 }),
  ];
  for (line, refusal) in &cases {
    assert_eq!(
      ClientLine::read(line.as_bytes(), ObjectFormat::Sha1),
      Err(*refusal),
      "{:?}",
      line.escape_debug()
    );
  }
  assert_eq!(
    ClientLine::read(format!("have {tip}").as_bytes(), ObjectFormat::Sha256),
    Err(LineRefusal::Id {
      at: 5,
      refusal: HexRefusal::Length { len: 40 },
    })
  );
}

/// A-604, refused by kind: do read server lines each malformed one way. Expect: each refused with its kind and the
/// byte where it stopped; an `ACK` with a word after its name that is none of git's three refused, never read as plain.
#[test]
fn malformed_server_lines_are_refused_by_kind_where_they_stop() {
  let tip = recorded("refs/heads/main").to_string();
  let cases = [
    (
      format!("ACK {tip} continuing"),
      LineRefusal::Form { at: 45 },
    ),
    (format!("ACK {tip} "), LineRefusal::Form { at: 45 }),
    (format!("ACK {tip}  common"), LineRefusal::Form { at: 45 }),
    (format!("ACK {tip} ready now"), LineRefusal::Form { at: 45 }),
    ("ACK".to_owned(), LineRefusal::Form { at: 3 }),
    (
      "ACK ".to_owned(),
      LineRefusal::Id {
        at: 4,
        refusal: HexRefusal::Length { len: 0 },
      },
    ),
    ("NAK ".to_owned(), LineRefusal::Form { at: 3 }),
    ("NAKED".to_owned(), LineRefusal::Unknown),
    (format!("ack {tip}"), LineRefusal::Unknown),
    ("ERR x\0".to_owned(), LineRefusal::Text { at: 5 }),
    (format!("shallow {tip}"), LineRefusal::Unknown),
  ];
  for (line, refusal) in &cases {
    assert_eq!(
      ServerLine::read(line.as_bytes(), ObjectFormat::Sha1),
      Err(*refusal),
      "{line:?}"
    );
  }
}

/// Where the packet holding `line` begins in a stream made by [`stream`] from `lines`.
fn packet_at(lines: &[&[u8]], index: usize) -> u64 {
  to_u64(lines[..index].iter().map(|line| line.len() + 4).sum())
}

/// A-604, refused by kind: do decode advertisements that break the grammar one way each. Expect: each refused with its
/// kind where its packet begins: a first line with no NUL or no capabilities, an `object-format` git does not know, a
/// name too short for SHA-256, a version other than 1, a peeled line not right after its ref, a ref after the shallow
/// lines, `capabilities^{}` with a name not null or not alone, a delimiter, `ERR`, a ref name with a space.
#[test]
fn advertisements_that_break_the_grammar_are_refused_by_kind() {
  let (tip, two) = (
    recorded("refs/heads/main").to_string(),
    commit("two").to_string(),
  );
  let first = format!("{tip} HEAD\0multi_ack\n");
  let null = "0".repeat(40);
  let refused_line = |lines: &[&[u8]], index: usize, refusal: LineRefusal| {
    let bytes = stream(lines);
    assert_eq!(
      Advertisement::decode(&bytes, &mut room()),
      Err(Refusal::Line {
        at: packet_at(lines, index),
        refusal
      }),
      "{:?}",
      bytes.escape_ascii().to_string()
    );
  };
  refused_line(
    &[format!("{tip} HEAD\n").as_bytes()],
    0,
    LineRefusal::Form { at: 45 },
  );
  refused_line(
    &[format!("{tip} HEAD\0\n").as_bytes()],
    0,
    LineRefusal::Capability { at: 46 },
  );
  refused_line(
    &[format!("{tip} HEAD\0multi_ack object-format=sha3\n").as_bytes()],
    0,
    LineRefusal::Format { at: 56 },
  );
  refused_line(
    &[format!("{tip} HEAD\0object-format=sha256\n").as_bytes()],
    0,
    LineRefusal::Id {
      at: 0,
      refusal: HexRefusal::Length { len: 40 },
    },
  );
  refused_line(
    &[
      first.as_bytes(),
      format!("{tip} refs/heads/a b\n").as_bytes(),
    ],
    1,
    LineRefusal::Name { at: 53 },
  );
  refused_line(
    &[first.as_bytes(), format!("shallow {two}x\n").as_bytes()],
    1,
    LineRefusal::Id {
      at: 8,
      refusal: HexRefusal::Length { len: 41 },
    },
  );
  let unexpected = |lines: &[&[u8]], index: usize| {
    let bytes = stream(lines);
    assert_eq!(
      Advertisement::decode(&bytes, &mut room()),
      Err(Refusal::Unexpected {
        at: packet_at(lines, index)
      }),
      "{:?}",
      bytes.escape_ascii().to_string()
    );
  };
  unexpected(
    &[
      first.as_bytes(),
      format!("{two} refs/tags/v1^{{}}\n").as_bytes(),
    ],
    1,
  );
  unexpected(
    &[
      first.as_bytes(),
      format!("{tip} refs/tags/v1\n").as_bytes(),
      format!("{tip} refs/tags/v2\n").as_bytes(),
      format!("{two} refs/tags/v1^{{}}\n").as_bytes(),
    ],
    3,
  );
  unexpected(
    &[
      first.as_bytes(),
      format!("shallow {two}\n").as_bytes(),
      format!("{tip} refs/heads/main\n").as_bytes(),
    ],
    2,
  );
  unexpected(
    &[format!("{tip} capabilities^{{}}\0multi_ack\n").as_bytes()],
    0,
  );
  unexpected(
    &[
      format!("{null} capabilities^{{}}\0multi_ack\n").as_bytes(),
      format!("{tip} refs/heads/main\n").as_bytes(),
    ],
    1,
  );
  unexpected(
    &[
      first.as_bytes(),
      format!("{null} capabilities^{{}}\n").as_bytes(),
    ],
    1,
  );
  unexpected(&[format!("{tip} HEAD^{{}}\0multi_ack\n").as_bytes()], 0);
  for (bytes, refusal) in [
    (b"0000".to_vec(), Refusal::Unexpected { at: 0 }),
    (stream(&[b"version 2\n"]), Refusal::Version { at: 0 }),
    (stream(&[b"version 0\n"]), Refusal::Version { at: 0 }),
    (
      stream(&[b"ERR access denied or repository not exported"]),
      Refusal::ServerError { at: 0 },
    ),
    (
      [packet(first.as_bytes()), b"0001".to_vec()].concat(),
      Refusal::Unexpected {
        at: to_u64(first.len() + 4),
      },
    ),
    (
      stream(&[first.as_bytes(), b"ERR gone\n"]),
      Refusal::ServerError {
        at: to_u64(first.len() + 4),
      },
    ),
    (
      b"zzzz".to_vec(),
      Refusal::Packet(pktline::Refusal::NotHex { at: 0 }),
    ),
  ] {
    assert_eq!(
      Advertisement::decode(&bytes, &mut room()),
      Err(refusal),
      "{:?}",
      bytes.escape_ascii().to_string()
    );
  }
}

/// A-604, refused by kind: do decode shallow-updates that break the grammar one way each. Expect: a `shallow` after an
/// `unshallow` and a delimiter refused as unexpected where their packets begin, a line of another kind refused as
/// unknown, a malformed name by its kind, `ERR` as the server's error.
#[test]
fn shallow_updates_that_break_the_grammar_are_refused_by_kind() {
  let (tip, two) = (
    recorded("refs/heads/main").to_string(),
    commit("two").to_string(),
  );
  let unshallow = format!("unshallow {tip}");
  let shallow = format!("shallow {two}");
  for (bytes, refusal) in [
    (
      stream(&[unshallow.as_bytes(), shallow.as_bytes()]),
      Refusal::Unexpected {
        at: to_u64(unshallow.len() + 4),
      },
    ),
    (
      stream(&[b"NAK\n"]),
      Refusal::Line {
        at: 0,
        refusal: LineRefusal::Unknown,
      },
    ),
    (
      stream(&[format!("{shallow}x").as_bytes()]),
      Refusal::Line {
        at: 0,
        refusal: LineRefusal::Id {
          at: 8,
          refusal: HexRefusal::Length { len: 41 },
        },
      },
    ),
    (
      stream(&[b"unshallow"]),
      Refusal::Line {
        at: 0,
        refusal: LineRefusal::Form { at: 9 },
      },
    ),
    (
      [packet(shallow.as_bytes()), b"0001".to_vec()].concat(),
      Refusal::Unexpected {
        at: to_u64(shallow.len() + 4),
      },
    ),
    (
      stream(&[b"ERR upload-pack: deepen and deepen-since cannot be used together"]),
      Refusal::ServerError { at: 0 },
    ),
  ] {
    assert_eq!(
      ShallowUpdate::decode(&bytes, ObjectFormat::Sha1, &mut room()),
      Err(refusal),
      "{:?}",
      bytes.escape_ascii().to_string()
    );
  }
}

/// A-604: do write lines and messages with a part a reader would refuse, and a line past a packet's data. Expect: each
/// refused by kind and nothing written, for a message even when its bad part comes after good lines.
#[test]
fn a_writer_refuses_what_a_reader_would_and_writes_nothing() {
  let tip = recorded("refs/heads/main");
  let wide = ObjectId::from_hex(ObjectFormat::Sha256, SHA256_TIP.as_bytes()).unwrap();
  let long = vec![b'x'; MAX_DATA];
  let bad_name = [Capability {
    name: b"Bad",
    value: None,
  }];
  let equals_in_name = [Capability {
    name: b"a=b",
    value: None,
  }];
  let lines = [
    (
      ClientLine::Deepen(0),
      WriteRefusal::Line(LineRefusal::Number { at: 7 }),
    ),
    (
      ClientLine::Deepen(1 << 31),
      WriteRefusal::Line(LineRefusal::Number { at: 7 }),
    ),
    (
      ClientLine::DeepenSince(u64::MAX),
      WriteRefusal::Line(LineRefusal::Number { at: 13 }),
    ),
    (
      ClientLine::DeepenNot(b"a b"),
      WriteRefusal::Line(LineRefusal::Name { at: 12 }),
    ),
    (
      ClientLine::Filter(b""),
      WriteRefusal::Line(LineRefusal::Text { at: 7 }),
    ),
    (
      ClientLine::Error(b"a\nb"),
      WriteRefusal::Line(LineRefusal::Text { at: 5 }),
    ),
    (
      ClientLine::Want {
        id: tip,
        capabilities: Some(Capabilities::listed(&[])),
      },
      WriteRefusal::Line(LineRefusal::Capability { at: 46 }),
    ),
    (
      ClientLine::Want {
        id: tip,
        capabilities: Some(Capabilities::listed(&bad_name)),
      },
      WriteRefusal::Line(LineRefusal::Capability { at: 46 }),
    ),
    (
      ClientLine::Want {
        id: tip,
        capabilities: Some(Capabilities::listed(&equals_in_name)),
      },
      WriteRefusal::Line(LineRefusal::Capability { at: 47 }),
    ),
    (
      ClientLine::Filter(&long),
      WriteRefusal::TooLong {
        len: to_u64(MAX_DATA + 7),
      },
    ),
  ];
  for (line, refusal) in lines {
    let mut out = Vec::new();
    assert_eq!(line.write(&mut out), Err(refusal), "{line:?}");
    assert!(out.is_empty(), "{line:?}");
  }
  let mut out = Vec::new();
  assert_eq!(
    ServerLine::Error(b"a\0").write(&mut out),
    Err(WriteRefusal::Line(LineRefusal::Text { at: 5 }))
  );
  assert!(out.is_empty());

  let (advertisement, _) = decoded(file("advertisement-v0.bin"));
  let with_ref = |index: usize, reference: AdvertisedRef<'static>| {
    let mut changed = advertisement.clone();
    changed.refs[index] = reference;
    changed
  };
  let unknown_format = [Capability {
    name: b"object-format",
    value: Some(b"sha3"),
  }];
  for (changed, refusal) in [
    (
      with_ref(
        3,
        AdvertisedRef {
          id: tip,
          name: b"refs/tags/v2^{}",
          peeled: None,
        },
      ),
      WriteRefusal::Line(LineRefusal::Name { at: 41 }),
    ),
    (
      with_ref(
        2,
        AdvertisedRef {
          id: tip,
          name: b"a b",
          peeled: None,
        },
      ),
      WriteRefusal::Line(LineRefusal::Name { at: 42 }),
    ),
    (
      with_ref(
        3,
        AdvertisedRef {
          id: tip,
          name: b"refs/tags/v1",
          peeled: Some(wide),
        },
      ),
      WriteRefusal::Format,
    ),
    (
      Advertisement {
        capabilities: Capabilities::listed(&unknown_format),
        ..advertisement.clone()
      },
      WriteRefusal::Format,
    ),
  ] {
    let mut out = Vec::new();
    assert_eq!(changed.write(&mut out), Err(refusal));
    assert!(out.is_empty(), "{refusal:?}: wrote {} bytes", out.len());
  }
  let mixed = ShallowUpdate {
    shallow: vec![tip],
    unshallow: vec![wide],
  };
  let mut out = Vec::new();
  assert_eq!(mixed.write(&mut out), Err(WriteRefusal::Format));
  assert!(out.is_empty());
}

/// A-604, bounds: do decode the v0 advertisement (four refs, one peeled line folded in) and the deepened exchange's
/// shallow-update (one `shallow`, one `unshallow`) under an admission one byte short of their lists, then under exactly
/// enough. Expect: refused `PastAdmission` with the admission untouched, then decoded with it spent to zero.
#[test]
fn a_decode_past_its_admission_is_refused_before_its_lists_are_made() {
  let bytes = file("advertisement-v0.bin");
  let lists = to_u64(4 * core::mem::size_of::<AdvertisedRef<'_>>());
  let mut short = Admission::new(lists - 1);
  assert_eq!(
    Advertisement::decode(bytes, &mut short),
    Err(Refusal::PastAdmission(PastAdmission {
      asked: lists,
      left: lists - 1
    }))
  );
  assert_eq!(short.left(), lists - 1);
  let mut exact = Admission::new(lists);
  assert!(Advertisement::decode(bytes, &mut exact).unwrap().is_some());
  assert_eq!(exact.left(), 0);

  let response = file("deepen-v0.response.bin");
  let (_, took) = decoded(response);
  let update = &response[took..];
  let lists = to_u64(2 * core::mem::size_of::<ObjectId>());
  let mut short = Admission::new(lists - 1);
  assert_eq!(
    ShallowUpdate::decode(update, ObjectFormat::Sha1, &mut short),
    Err(Refusal::PastAdmission(PastAdmission {
      asked: lists,
      left: lists - 1
    }))
  );
  assert_eq!(short.left(), lists - 1);
  let mut exact = Admission::new(lists);
  assert!(
    ShallowUpdate::decode(update, ObjectFormat::Sha1, &mut exact)
      .unwrap()
      .is_some()
  );
  assert_eq!(exact.left(), 0);
}

/// Every shallow-update stock git wrote, alone: the bytes after the advertisement in each exchange that asked for
/// depth, and the stateless `unshallow` answer's first message.
fn shallow_updates() -> Vec<(&'static str, &'static [u8])> {
  let mut updates = Vec::new();
  for name in ["depth-v0", "deepen-v0", "since-v0", "exclude-v0"] {
    let response = file(&format!("{name}.response.bin"));
    let (_, took) = decoded(response);
    updates.push((name, &response[took..]));
  }
  updates.push(("unshallow", file("unshallow.response.bin")));
  updates
    .into_iter()
    .map(|(name, bytes)| {
      let (_, took) = ShallowUpdate::decode(bytes, ObjectFormat::Sha1, &mut room())
        .unwrap()
        .unwrap();
      (name, &bytes[..took])
    })
    .collect()
}

/// Every line stock git wrote in a request or a negotiation, each with whether it is a client's: the data of every
/// data packet of every request, and of every answer's packets after its advertisement and shallow-update and before
/// its pack.
fn every_line() -> Vec<(bool, &'static [u8])> {
  let mut lines = Vec::new();
  for (name, advertised) in EXCHANGES {
    let request = file(&format!("{name}.request.bin"));
    for packet in packets(request) {
      if let Packet::Data(data) = packet {
        lines.push((true, data));
      }
    }
    let mut rest = file(&format!("{name}.response.bin"));
    if advertised {
      rest = &rest[decoded(rest).1..];
    }
    if let Ok(Some((_, took))) = ShallowUpdate::decode(rest, ObjectFormat::Sha1, &mut room()) {
      rest = &rest[took..];
    }
    for packet in packets(rest) {
      match packet {
        Packet::Data(data) if !matches!(data.first(), Some(1..=3)) => lines.push((false, data)),
        _ => break,
      }
    }
  }
  lines
}

/// The data a line writes, or `None` when the line is refused, read as a client's or a server's.
fn reread(client: bool, data: &[u8]) -> Option<Vec<u8>> {
  let mut out = Vec::new();
  if client {
    let line = ClientLine::read(data, ObjectFormat::Sha1).ok()?;
    line.write(&mut out).unwrap();
  } else {
    let line = ServerLine::read(data, ObjectFormat::Sha1).ok()?;
    line.write(&mut out).unwrap();
  }
  Some(data_of(&out).to_vec())
}

/// A-604, hostile input: do decode every cut of every advertisement and shallow-update stock git wrote, and read every
/// cut of every line it wrote. Expect: every message cut short answers `None` (not yet whole) and never a refusal or a
/// message; every line cut short is refused or reads as exactly the bytes it kept, written back to them, never as more.
#[test]
fn every_cut_of_a_message_waits_and_every_cut_of_a_line_reads_as_its_bytes_say() {
  for name in ADVERTISEMENTS {
    let bytes = file(name);
    for cut in 0..bytes.len() {
      assert_eq!(
        Advertisement::decode(&bytes[..cut], &mut room()),
        Ok(None),
        "{name} cut at {cut}"
      );
    }
  }
  for (name, bytes) in shallow_updates() {
    for cut in 0..bytes.len() {
      assert_eq!(
        ShallowUpdate::decode(&bytes[..cut], ObjectFormat::Sha1, &mut room()),
        Ok(None),
        "{name} cut at {cut}"
      );
    }
  }
  let mut read = 0;
  for (client, data) in every_line() {
    for cut in 0..data.len() {
      let kept = &data[..cut];
      if let Some(again) = reread(client, kept) {
        assert_eq!(chomped(&again), chomped(kept), "{:?}", kept.escape_ascii());
        read += 1;
      }
    }
  }
  assert!(read > 0, "some cuts are whole lines of their own");
}

/// Whether the flip at `index` made an upper-case hexadecimal digit of a lower-case one: an object name's digit, read
/// the same in either case.
fn upper_case_flip(original: &[u8], flipped: &[u8], index: usize) -> bool {
  let byte = flipped[index];
  byte.is_ascii_hexdigit()
    && byte.is_ascii_uppercase()
    && original[index] == byte.to_ascii_lowercase()
}

/// What the flips of one test reached.
#[derive(Debug, Default)]
struct Reached {
  read: u32,
  waiting: u32,
  packet: u32,
  line: u32,
  unexpected: u32,
}

/// A-604, hostile input: do flip every bit of every byte of every advertisement and shallow-update stock git wrote, and
/// of every line, and decode each. Expect: no panic; every refusal typed, at a place inside the bytes; and whatever
/// still decodes reads as its bytes say: written back to them, or to the original where the flip made an object name's
/// digit upper case, or, where the flip took the LF off a line, to them with that LF again.
#[test]
fn every_bit_flip_is_refused_by_kind_or_read_as_its_bytes_say() {
  let mut reached = Reached::default();
  for name in ADVERTISEMENTS {
    let bytes = file(name);
    for index in 0..bytes.len() {
      for bit in 0..8 {
        let mut flipped = bytes.to_vec();
        flipped[index] ^= 1 << bit;
        match Advertisement::decode(&flipped, &mut room()) {
          Ok(Some((advertisement, took))) => {
            let again = written(|out| advertisement.write(out));
            let as_read = again == flipped[..took]
              || (upper_case_flip(bytes, &flipped, index) && again == bytes[..took])
              || (bytes[index] == b'\n'
                && again.len() == took + 1
                && decoded(&again).0 == advertisement);
            assert!(as_read, "{name} {index}.{bit}");
            reached.read += 1;
          }
          Ok(None) => reached.waiting += 1,
          Err(refusal) => reached.count(refusal, flipped.len()),
        }
      }
    }
  }
  for (name, bytes) in shallow_updates() {
    for index in 0..bytes.len() {
      for bit in 0..8 {
        let mut flipped = bytes.to_vec();
        flipped[index] ^= 1 << bit;
        match ShallowUpdate::decode(&flipped, ObjectFormat::Sha1, &mut room()) {
          Ok(Some((update, took))) => {
            let again = written(|out| update.write(out));
            assert!(
              again == flipped[..took]
                || (upper_case_flip(bytes, &flipped, index) && again == bytes[..took]),
              "{name} {index}.{bit}"
            );
            reached.read += 1;
          }
          Ok(None) => reached.waiting += 1,
          Err(refusal) => reached.count(refusal, flipped.len()),
        }
      }
    }
  }
  for (client, data) in every_line() {
    for index in 0..data.len() {
      for bit in 0..8 {
        let mut flipped = data.to_vec();
        flipped[index] ^= 1 << bit;
        if let Some(again) = reread(client, &flipped) {
          assert!(
            chomped(&again) == chomped(&flipped)
              || (upper_case_flip(data, &flipped, index) && chomped(&again) == chomped(data)),
            "{:?} {index}.{bit}",
            data.escape_ascii()
          );
        }
      }
    }
  }
  assert!(
    reached.read > 0
      && reached.waiting > 0
      && reached.packet > 0
      && reached.line > 0
      && reached.unexpected > 0,
    "the flips reached every outcome: {reached:?}"
  );
}

impl Reached {
  /// Counts a refusal, checking its place is inside the `len` bytes decoded.
  fn count(&mut self, refusal: Refusal, len: usize) {
    let at = match refusal {
      Refusal::Packet(_) => {
        self.packet += 1;
        return;
      }
      Refusal::Line { at, .. } => {
        self.line += 1;
        at
      }
      Refusal::Unexpected { at } | Refusal::Version { at } | Refusal::ServerError { at } => {
        self.unexpected += 1;
        at
      }
      Refusal::PastAdmission(past) => panic!("{past:?} past a room no message exceeds"),
    };
    assert!(usize::try_from(at).unwrap() < len);
  }
}

/// A capability a generated list may hold: a name in the grammar, and maybe a value of bytes a ref name may hold.
fn any_capability() -> impl Strategy<Value = (Vec<u8>, Option<Vec<u8>>)> {
  let name_byte = prop_oneof![b'a'..=b'z', b'0'..=b'9', Just(b'-'), Just(b'_')];
  (
    proptest::collection::vec(name_byte, 1..12),
    proptest::option::of(proptest::collection::vec(any_word_byte(), 0..12)),
  )
}

/// A byte a ref name may hold: above space, not DEL.
fn any_word_byte() -> impl Strategy<Value = u8> {
  (b'!'..=u8::MAX).prop_filter("not DEL", |byte| *byte != 0x7f)
}

/// A byte a text may hold: any but NUL and LF.
fn any_text_byte() -> impl Strategy<Value = u8> {
  any::<u8>().prop_filter("not NUL or LF", |byte| *byte != 0 && *byte != b'\n')
}

/// An object name in `format`.
fn any_id(format: ObjectFormat) -> impl Strategy<Value = ObjectId> {
  proptest::collection::vec(any::<u8>(), format.raw_len())
    .prop_map(move |bytes| ObjectId::from_bytes(format, &bytes).unwrap())
}

/// Either object format.
fn any_format() -> impl Strategy<Value = ObjectFormat> {
  prop_oneof![Just(ObjectFormat::Sha1), Just(ObjectFormat::Sha256)]
}

/// A client line's parts, owned, from which a test borrows the line.
#[derive(Clone, Debug)]
enum OwnedClientLine {
  Want(ObjectId, Option<Vec<(Vec<u8>, Option<Vec<u8>>)>>),
  Have(ObjectId),
  Done,
  Shallow(ObjectId),
  Deepen(u32),
  DeepenSince(u64),
  DeepenNot(Vec<u8>),
  Filter(Vec<u8>),
  Error(Vec<u8>),
}

/// Any client line a writer takes, in `format`.
fn any_client_line(format: ObjectFormat) -> impl Strategy<Value = OwnedClientLine> {
  prop_oneof![
    (
      any_id(format),
      proptest::option::of(proptest::collection::vec(any_capability(), 1..6))
    )
      .prop_map(|(id, capabilities)| OwnedClientLine::Want(id, capabilities)),
    any_id(format).prop_map(OwnedClientLine::Have),
    Just(OwnedClientLine::Done),
    any_id(format).prop_map(OwnedClientLine::Shallow),
    (1..=i32::MAX.unsigned_abs()).prop_map(OwnedClientLine::Deepen),
    (0..u64::MAX).prop_map(OwnedClientLine::DeepenSince),
    proptest::collection::vec(any_word_byte(), 1..20).prop_map(OwnedClientLine::DeepenNot),
    proptest::collection::vec(any_text_byte(), 1..20).prop_map(OwnedClientLine::Filter),
    proptest::collection::vec(any_text_byte(), 0..20).prop_map(OwnedClientLine::Error),
  ]
}

/// The capabilities `owned` lists, borrowed.
fn borrowed(owned: &[(Vec<u8>, Option<Vec<u8>>)]) -> Vec<Capability<'_>> {
  owned
    .iter()
    .map(|(name, value)| Capability {
      name,
      value: value.as_deref(),
    })
    .collect()
}

/// A ref a generated advertisement may hold: its name not a peeled line's, maybe a peeled object.
fn any_ref(format: ObjectFormat) -> impl Strategy<Value = (ObjectId, Vec<u8>, Option<ObjectId>)> {
  (
    any_id(format),
    proptest::collection::vec(any_word_byte(), 1..24)
      .prop_filter("not a peeled line's name", |name| !name.ends_with(b"^{}")),
    proptest::option::of(any_id(format)),
  )
}

/// An advertisement's parts, owned: its version, its capabilities (an `object-format` naming its format added), its
/// refs and its shallow commits.
fn any_advertisement() -> impl Strategy<
  Value = (
    Version,
    Vec<(Vec<u8>, Option<Vec<u8>>)>,
    Vec<(ObjectId, Vec<u8>, Option<ObjectId>)>,
    Vec<ObjectId>,
  ),
> {
  any_format().prop_flat_map(|format| {
    (
      prop_oneof![Just(Version::V0), Just(Version::V1)],
      proptest::collection::vec(
        any_capability().prop_filter("object-format is the format's", |(name, _)| {
          name != b"object-format"
        }),
        0..6,
      )
      .prop_map(move |mut capabilities| {
        capabilities.push((
          b"object-format".to_vec(),
          Some(format.name().as_bytes().to_vec()),
        ));
        capabilities
      }),
      proptest::collection::vec(any_ref(format), 0..6),
      proptest::collection::vec(any_id(format), 0..4),
    )
  })
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-604: do write any client line a writer takes, in either format, and read it back. Expect: one packet, read as
  /// the same line; ended by LF exactly when git ends that line so (`want`, `have`, `done`).
  #[test]
  fn any_client_line_written_reads_back_as_itself(owned in any_format().prop_flat_map(any_client_line)) {
    let capabilities = match &owned {
      OwnedClientLine::Want(_, Some(list)) => borrowed(list),
      _ => Vec::new(),
    };
    let line = match &owned {
      OwnedClientLine::Want(id, list) => ClientLine::Want {
        id: *id,
        capabilities: list.as_ref().map(|_| Capabilities::listed(&capabilities)),
      },
      OwnedClientLine::Have(id) => ClientLine::Have(*id),
      OwnedClientLine::Done => ClientLine::Done,
      OwnedClientLine::Shallow(id) => ClientLine::Shallow(*id),
      OwnedClientLine::Deepen(depth) => ClientLine::Deepen(*depth),
      OwnedClientLine::DeepenSince(time) => ClientLine::DeepenSince(*time),
      OwnedClientLine::DeepenNot(name) => ClientLine::DeepenNot(name),
      OwnedClientLine::Filter(spec) => ClientLine::Filter(spec),
      OwnedClientLine::Error(text) => ClientLine::Error(text),
    };
    let format = match &owned {
      OwnedClientLine::Want(id, _) | OwnedClientLine::Have(id) | OwnedClientLine::Shallow(id) => id.format(),
      _ => ObjectFormat::Sha1,
    };
    let bytes = written(|out| line.write(out));
    let data = data_of(&bytes);
    prop_assert_eq!(ClientLine::read(data, format), Ok(line));
    let ends_in_lf = matches!(line, ClientLine::Want { .. } | ClientLine::Have(_) | ClientLine::Done);
    prop_assert_eq!(data.ends_with(b"\n"), ends_in_lf);
  }

  /// A-604: do write any server line, in either format, and read it back. Expect: read as the same line.
  #[test]
  fn any_server_line_written_reads_back_as_itself(
    id in any_format().prop_flat_map(any_id),
    status in prop_oneof![
      Just(AckStatus::Plain), Just(AckStatus::Continue), Just(AckStatus::Common), Just(AckStatus::Ready)
    ],
    text in proptest::collection::vec(any_text_byte(), 0..30),
  ) {
    for line in [ServerLine::Nak, ServerLine::Ack { id, status }, ServerLine::Error(&text)] {
      let bytes = written(|out| line.write(out));
      prop_assert_eq!(ServerLine::read(data_of(&bytes), id.format()), Ok(line));
    }
  }

  /// A-604: do write any advertisement and any shallow-update a writer takes and decode each. Expect: the same
  /// advertisement, every byte taken, its refs, peeled objects and shallow commits in their order, an empty one sent as
  /// `capabilities^{}`; the same shallow-update.
  #[test]
  fn any_advertisement_and_shallow_update_written_decode_back_to_themselves(
    (version, capabilities, refs, shallows) in any_advertisement(),
    unshallow in any_format().prop_flat_map(|format| proptest::collection::vec(any_id(format), 0..4)),
  ) {
    let listed = borrowed(&capabilities);
    let advertisement = Advertisement {
      version,
      capabilities: Capabilities::listed(&listed),
      refs: refs
        .iter()
        .map(|(id, name, peeled)| AdvertisedRef { id: *id, name, peeled: *peeled })
        .collect(),
      shallows: shallows.clone(),
    };
    let bytes = written(|out| advertisement.write(out));
    prop_assert_eq!(decoded(&bytes), (advertisement.clone(), bytes.len()));
    if advertisement.refs.is_empty() {
      prop_assert!(bytes.windows(16).any(|window| window == b"capabilities^{}\0"));
    }
    let format = unshallow.first().map_or(ObjectFormat::Sha1, ObjectId::format);
    let update = ShallowUpdate { shallow: Vec::new(), unshallow };
    let bytes = written(|out| update.write(out));
    prop_assert_eq!(
      ShallowUpdate::decode(&bytes, format, &mut room()),
      Ok(Some((update, bytes.len())))
    );
  }
}
