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
const FILES: [(&str, &[u8]); 47] = [
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
  fixture!("sparse-bare-v0.request.bin"),
  fixture!("sparse-bare-v0.response.bin"),
  fixture!("sparse-missing-v0.request.bin"),
  fixture!("sparse-missing-v0.response.bin"),
  fixture!("sparse-v0.request.bin"),
  fixture!("sparse-v0.response.bin"),
  fixture!("sparse-warn-v0.request.bin"),
  fixture!("sparse-warn-v0.response.bin"),
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

/// `upload-pack`'s refusal of the want it never advertised.
const NOT_OUR_REF: &[u8] = b"upload-pack: not our ref 86e52f206da93ad5319625df790d3a5ed9f57f60";

/// The SHA-256 repository's one commit, as its advertisement names it.
const SHA256_TIP: &str = "72c9f67255c8696e8ab06b03065d9fffbf200461a1ef966ee954cb1de52d5331";

/// The capabilities stock `upload-pack` advertises that the tests look for.
const UPLOAD_CAPABILITIES: [&[u8]; 7] = [
  b"multi_ack",
  b"multi_ack_detailed",
  b"side-band-64k",
  b"shallow",
  b"deepen-since",
  b"no-done",
  b"filter",
];

/// The capabilities stock `receive-pack` advertises that the tests look for.
const RECEIVE_CAPABILITIES: [&[u8]; 5] = [
  b"report-status",
  b"report-status-v2",
  b"delete-refs",
  b"atomic",
  b"side-band-64k",
];

/// Format: a pack's first eight bytes, its signature and version 2 (gitformat-pack).
const PACK_HEAD: &[u8] = b"PACK\0\0\0\x02";

/// The capabilities `fetch-pack` chose for a clone of the server.
const CLONE: &str = "multi_ack_detailed side-band-64k thin-pack no-progress ofs-delta deepen-since deepen-not agent=git/2.50.1-Darwin";

/// The capabilities `fetch-pack` chose for a fetch that follows tags.
const FETCH: &str = "multi_ack_detailed side-band-64k thin-pack no-progress include-tag ofs-delta deepen-since \
deepen-not agent=git/2.50.1-Darwin";

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

/// What the server's `upload-pack` advertises: `HEAD` at `main`'s commit, then every ref `refs.txt` recorded, the tag
/// `v1` peeled to `two`, which `log.txt` recorded.
fn recorded_advertisement() -> Vec<AdvertisedRef<'static>> {
  let head = AdvertisedRef {
    id: recorded("refs/heads/main"),
    name: b"HEAD",
    peeled: None,
  };
  let refs = recorded_refs().into_iter().map(|(name, id)| AdvertisedRef {
    id,
    name,
    peeled: (name == b"refs/tags/v1").then(|| commit("two")),
  });
  std::iter::once(head).chain(refs).collect()
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

/// The whole shallow-update `bytes` begin with.
fn decoded_update(bytes: &[u8]) -> (ShallowUpdate, usize) {
  ShallowUpdate::decode(bytes, ObjectFormat::Sha1, &mut room())
    .unwrap()
    .expect("a whole shallow-update")
}

/// What `write` writes, which must not be refused.
fn written(write: impl FnOnce(&mut Vec<u8>) -> Result<(), WriteRefusal>) -> Vec<u8> {
  let mut out = Vec::new();
  write(&mut out).unwrap();
  out
}

/// What `write` answers, and how many bytes it wrote.
fn attempt(
  write: impl FnOnce(&mut Vec<u8>) -> Result<(), WriteRefusal>,
) -> (Result<(), WriteRefusal>, usize) {
  let mut out = Vec::new();
  let result = write(&mut out);
  (result, out.len())
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

/// Whether a data packet is a pack's side-band packet: its first byte names band 1, 2 or 3.
fn on_a_band(data: &[u8]) -> bool {
  data.first().is_some_and(|byte| Band::of(*byte).is_some())
}

/// Where the packet holding the line at `index` begins in a stream made by [`stream`] from `lines`.
fn packet_at(lines: &[&[u8]], index: usize) -> u64 {
  to_u64(lines[..index].iter().map(|line| line.len() + 4).sum())
}

/// Reads a request line by line (`None` for a flush), checking that each line writes back to its packet's bytes.
fn read_request<'a>(name: &str, request: &'a [u8]) -> Vec<Option<ClientLine<'a>>> {
  let mut read = Vec::new();
  let mut again = Vec::new();
  for received in packets(request) {
    let line = match received {
      Packet::Data(data) => Some(
        ClientLine::read(data, ObjectFormat::Sha1)
          .unwrap_or_else(|refusal| panic!("{name}: {refusal:?} in {:?}", data.escape_ascii())),
      ),
      _ => None,
    };
    match line {
      Some(line) => line.write(&mut again).unwrap(),
      None => pktline::write(received, &mut again).unwrap(),
    }
    read.push(line);
  }
  assert_eq!(again, request, "{name}: the request written back");
  read
}

/// What a server answered in one exchange.
struct Answer<'a> {
  shallow_update: Option<ShallowUpdate>,
  lines: Vec<ServerLine<'a>>,
  pack: Vec<u8>,
}

/// Reads an answer: the advertisement when it opens with one, the shallow-update when the request asked for depth, the
/// server's lines, then the pack on side-band up to its flush. Every message and line is checked to write back to its
/// bytes.
fn read_answer<'a>(
  name: &str,
  request: &[Option<ClientLine<'_>>],
  response: &'a [u8],
  advertised: bool,
) -> Answer<'a> {
  let mut at = 0;
  if advertised {
    let (advertisement, took) = decoded(response);
    assert_eq!(
      written(|out| advertisement.write(out)),
      &response[..took],
      "{name}"
    );
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
    let (update, took) = decoded_update(&response[at..]);
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
  for received in packets(rest) {
    match received {
      Packet::Data(data) if !in_pack && !on_a_band(data) => {
        let line = ServerLine::read(data, ObjectFormat::Sha1)
          .unwrap_or_else(|refusal| panic!("{name}: {refusal:?}"));
        assert_eq!(written(|out| line.write(out)), packet(data), "{name}");
        lines.push(line);
      }
      Packet::Data(data) => {
        in_pack = true;
        let (band, bytes) = sideband::demux(data).unwrap();
        pack.extend_from_slice(if band == Band::Data { bytes } else { &[] });
      }
      other => assert_eq!((in_pack, other), (true, Packet::Flush), "{name}"),
    }
  }
  (lines, pack)
}

/// A first `want`: its object, and the capabilities it carries.
fn first_want(id: ObjectId, capabilities: &'static str) -> Option<ClientLine<'static>> {
  Some(ClientLine::Want {
    id,
    capabilities: Some(Capabilities::read(capabilities.as_bytes()).unwrap()),
  })
}

/// A further `want`, with no capabilities.
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

/// A shallow-update of `shallow` then `unshallow`.
fn update(shallow: &[ObjectId], unshallow: &[ObjectId]) -> Option<ShallowUpdate> {
  Some(ShallowUpdate {
    shallow: shallow.to_vec(),
    unshallow: unshallow.to_vec(),
  })
}

/// What an exchange holds: its request line by line (`None` for a flush), its shallow-update when it asked for depth,
/// and the server's lines before the pack.
type Expected = (
  Vec<Option<ClientLine<'static>>>,
  Option<ShallowUpdate>,
  Vec<ServerLine<'static>>,
);

/// What each exchange stock git had holds, by its name: requests as `fetch-pack` wrote them or the script did, answers
/// as git's negotiation rules give them.
fn expected(name: &str) -> Expected {
  let (tip, side, tag) = (
    recorded("refs/heads/main"),
    recorded("refs/heads/side"),
    recorded("refs/tags/v1"),
  );
  let (two, one, unknown) = (commit("two"), commit("one"), sha1(UNKNOWN));
  let done = Some(ClientLine::Done);
  let have = |id| Some(ClientLine::Have(id));
  match name {
    "clone-v0" | "clone-v1" => (
      vec![
        first_want(tip, CLONE),
        want(tip),
        want(side),
        want(tag),
        None,
        done,
      ],
      None,
      vec![ServerLine::Nak],
    ),
    "fetch-v0" | "fetch-v1" => (
      vec![first_want(tip, FETCH), None, have(two), done],
      None,
      vec![ack(two, AckStatus::Common), ack(two, AckStatus::Plain)],
    ),
    "depth-v0" => (
      vec![
        first_want(tip, FETCH),
        want(tip),
        Some(ClientLine::Deepen(1)),
        None,
        done,
      ],
      update(&[tip], &[]),
      vec![ServerLine::Nak],
    ),
    "deepen-v0" => (
      vec![
        first_want(
          tip,
          "multi_ack_detailed side-band-64k deepen-relative thin-pack no-progress ofs-delta deepen-since deepen-not \
agent=git/2.50.1-Darwin",
        ),
        Some(ClientLine::Shallow(tip)),
        Some(ClientLine::Deepen(1)),
        None,
        have(tip),
        done,
      ],
      update(&[two], &[tip]),
      vec![ack(tip, AckStatus::Common), ack(tip, AckStatus::Plain)],
    ),
    "since-v0" => (
      vec![
        first_want(tip, FETCH),
        want(tip),
        Some(ClientLine::DeepenSince(1_112_911_993)),
        None,
        done,
      ],
      update(&[], &[]),
      vec![ServerLine::Nak],
    ),
    "exclude-v0" => (
      vec![
        first_want(tip, FETCH),
        want(tip),
        Some(ClientLine::DeepenNot(b"side")),
        None,
        done,
      ],
      update(&[tip], &[]),
      vec![ServerLine::Nak],
    ),
    "filter-v0" => (
      vec![
        first_want(
          tip,
          "multi_ack_detailed side-band-64k thin-pack no-progress ofs-delta deepen-since deepen-not \
agent=git/2.50.1-Darwin filter",
        ),
        want(tip),
        want(side),
        want(tag),
        Some(ClientLine::Filter(b"blob:none")),
        None,
        done,
      ],
      None,
      vec![ServerLine::Nak],
    ),
    "multi-ack" => (
      vec![
        first_want(tip, "multi_ack side-band-64k ofs-delta"),
        None,
        have(unknown),
        have(two),
        have(one),
        None,
      ],
      None,
      vec![
        ack(two, AckStatus::Continue),
        ack(one, AckStatus::Continue),
        ServerLine::Nak,
      ],
    ),
    "plain" => (
      vec![
        first_want(tip, "side-band-64k ofs-delta"),
        None,
        have(unknown),
        have(two),
        have(one),
        None,
      ],
      None,
      vec![ack(two, AckStatus::Plain), ack(one, AckStatus::Plain)],
    ),
    "detailed-done" => (
      vec![
        first_want(
          tip,
          "multi_ack_detailed side-band-64k ofs-delta no-progress",
        ),
        want(side),
        None,
        have(unknown),
        have(two),
        done,
      ],
      None,
      vec![ack(two, AckStatus::Common), ack(two, AckStatus::Plain)],
    ),
    "ready" => (
      vec![
        first_want(tip, "multi_ack_detailed side-band-64k ofs-delta"),
        None,
        have(two),
        None,
      ],
      None,
      vec![
        ack(two, AckStatus::Common),
        ack(two, AckStatus::Ready),
        ServerLine::Nak,
      ],
    ),
    "unshallow" => (
      vec![
        first_want(
          tip,
          "multi_ack_detailed side-band-64k ofs-delta no-progress",
        ),
        Some(ClientLine::Shallow(tip)),
        Some(ClientLine::Deepen(2)),
        None,
        done,
      ],
      update(&[two], &[tip]),
      vec![ServerLine::Nak],
    ),
    "unknown-want" => (
      vec![first_want(unknown, "side-band-64k ofs-delta"), None, done],
      None,
      vec![ServerLine::Error(NOT_OUR_REF)],
    ),
    other => panic!("no expectations for {other}"),
  }
}

/// A-604, golden vectors: do hash every file `make-v0-fixtures.sh` wrote with SHA-256 (aws-lc-rs) and compare each
/// with the sum the script recorded. Expect: every sum equal and every file listed, so the bytes these tests read are
/// the ones stock git 2.50.1 wrote.
#[test]
fn every_golden_vector_is_the_file_the_script_recorded() {
  let mut listed = Vec::new();
  for line in SUMS.lines() {
    let (sum, name) = line.split_once("  ").expect("a sum and a name");
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, file(name));
    let hex = ObjectId::from_bytes(ObjectFormat::Sha256, digest.as_ref())
      .unwrap()
      .to_string();
    assert_eq!(hex, sum, "{name}");
    listed.push(name);
  }
  let mut names: Vec<&str> = FILES.iter().map(|(name, _)| *name).collect();
  names.sort_unstable();
  assert_eq!(
    (listed, file("git-version.txt")),
    (names, &b"git version 2.50.1 (Apple Git-155)\n"[..])
  );
}

/// A-604, golden vectors: do decode the advertisement stock git's `upload-pack` wrote under protocol v0 and under v1,
/// and write each back. Expect: v1's opens with `version 1` and is v0's after it; `HEAD` first, then every ref
/// `refs.txt` recorded with the object it names, the annotated tag `v1` with its peeled commit (`two`); no shallow
/// commit; the format SHA-1, `symref=HEAD:refs/heads/main`, git's agent and upload-pack's capabilities; the whole
/// stream taken; every byte written back.
#[test]
fn upload_packs_advertisement_under_v0_and_v1_reads_as_the_refs_recorded_and_writes_back() {
  let expected = recorded_advertisement();
  for (name, version) in [
    ("advertisement-v0.bin", Version::V0),
    ("advertisement-v1.bin", Version::V1),
  ] {
    let bytes = file(name);
    let (advertisement, took) = decoded(bytes);
    let capabilities = advertisement.capabilities;
    assert_eq!(
      (
        took,
        advertisement.version,
        &advertisement.refs,
        advertisement.shallows.is_empty(),
        capabilities.object_format(),
        capabilities.get(b"symref").and_then(|symref| symref.value),
      ),
      (
        bytes.len(),
        version,
        &expected,
        true,
        Ok(ObjectFormat::Sha1),
        Some(&b"HEAD:refs/heads/main"[..]),
      ),
      "{name}"
    );
    let agent = capabilities.get(b"agent").and_then(|agent| agent.value);
    assert!(
      agent.is_some_and(|agent| agent.starts_with(b"git/2.50.1")),
      "{agent:?}"
    );
    let missing: Vec<&&[u8]> = UPLOAD_CAPABILITIES
      .iter()
      .filter(|known| capabilities.get(known).is_none())
      .collect();
    assert!(missing.is_empty(), "{name}: {missing:?}");
    assert_eq!(written(|out| advertisement.write(out)), bytes, "{name}");
  }
}

/// A-604, golden vectors: do decode the advertisement of an empty repository and of a shallow one, and write each back.
/// Expect: the empty one has no refs, its `capabilities^{}` line carrying the capabilities after the null name and no
/// symref; the shallow one names its shallow commit (`two`) after its refs; every byte written back.
#[test]
fn the_empty_and_the_shallow_repositorys_advertisements_read_and_write_back() {
  let empty = file("advertisement-empty.bin");
  let (advertisement, _) = decoded(empty);
  assert_eq!(
    (
      advertisement.refs.is_empty(),
      advertisement.capabilities.get(b"multi_ack").is_some(),
      advertisement.capabilities.get(b"symref"),
      empty[4..].starts_with(b"0000000000000000000000000000000000000000 capabilities^{}\0"),
    ),
    (true, true, None, true)
  );
  assert_eq!(written(|out| advertisement.write(out)), empty);
  let shallow = file("advertisement-shallow.bin");
  let (advertisement, _) = decoded(shallow);
  assert_eq!(
    (&advertisement.shallows[..], advertisement.refs[0].name),
    (&[commit("two")][..], &b"HEAD"[..])
  );
  assert_eq!(written(|out| advertisement.write(out)), shallow);
}

/// A-604, golden vectors: do decode a SHA-256 repository's advertisement and `receive-pack`'s, and write each back.
/// Expect: the SHA-256 one reads its 64-digit names in the format its `object-format=sha256` names; `receive-pack`'s
/// carries its own capabilities on its first ref, `refs/heads/main`, names every ref `refs.txt` recorded and peels
/// nothing; every byte written back.
#[test]
fn a_sha256_and_receive_packs_advertisements_read_and_write_back() {
  let sha256 = file("advertisement-sha256.bin");
  let (advertisement, _) = decoded(sha256);
  let tip = ObjectId::from_hex(ObjectFormat::Sha256, SHA256_TIP.as_bytes()).unwrap();
  let names: Vec<(&[u8], ObjectId)> = advertisement
    .refs
    .iter()
    .map(|reference| (reference.name, reference.id))
    .collect();
  let expected: [(&[u8], ObjectId); 2] = [(b"HEAD", tip), (b"refs/heads/main", tip)];
  assert_eq!(
    (advertisement.capabilities.object_format(), &names[..]),
    (Ok(ObjectFormat::Sha256), &expected[..])
  );
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
  let missing: Vec<&&[u8]> = RECEIVE_CAPABILITIES
    .iter()
    .filter(|known| advertisement.capabilities.get(known).is_none())
    .collect();
  assert_eq!((&advertisement.refs, missing.len()), (&expected, 0));
  assert_eq!(written(|out| advertisement.write(out)), receive);
}

/// A-604, golden vectors: do read every exchange stock `git fetch-pack` and `git upload-pack` had, and every stateless
/// request and answer, line by line, writing each line and message back. Expect: every byte of every request and
/// answer read as a line of its kind and written back; each request as `fetch-pack` (or the script) wrote it, v0's and
/// v1's alike; each answer as git's negotiation rules give it (a clone's `NAK`; a fetch's `ACK common` then its last
/// `ACK`; `multi_ack`'s `continue`s and `NAK`; `ready`; the plain mode's `ACK`s and no `NAK`); each depth request
/// answered by its shallow-update; the unknown want refused by `ERR`; a pack on band 1 after every `done` the server
/// did not refuse, and none otherwise.
#[test]
fn every_exchange_reads_line_by_line_as_git_wrote_it_and_writes_back() {
  for (name, advertised) in EXCHANGES {
    let request = read_request(name, file(&format!("{name}.request.bin")));
    let answer = read_answer(
      name,
      &request,
      file(&format!("{name}.response.bin")),
      advertised,
    );
    let refused = answer
      .lines
      .iter()
      .any(|line| matches!(line, ServerLine::Error(_)));
    let pack_due = request.contains(&Some(ClientLine::Done)) && !refused;
    assert_eq!(
      (
        request,
        answer.shallow_update,
        answer.lines,
        answer.pack.starts_with(PACK_HEAD),
        answer.pack.is_empty(),
      ),
      {
        let (request, shallow_update, lines) = expected(name);
        (request, shallow_update, lines, pack_due, !pack_due)
      },
      "{name}"
    );
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
  let names: Vec<&[u8]> = advertisement
    .refs
    .iter()
    .map(|reference| reference.name)
    .collect();
  let expected_names: [&[u8]; 5] = [
    b"HEAD",
    b"refs/heads/integration",
    b"refs/heads/master",
    b"refs/tags/v0.9",
    b"refs/tags/v1.0",
  ];
  assert_eq!(
    (
      took,
      advertisement.version,
      &names[..],
      advertisement.refs[3].peeled,
      advertisement.refs[4].peeled,
      advertisement.capabilities.iter().count(),
      advertisement.capabilities.object_format(),
    ),
    (
      DOCUMENT_ADVERTISEMENT.len(),
      Version::V1,
      &expected_names[..],
      None,
      Some(sha1("e92df48743b7bc7d26bcaabfddde0a1e20cae47c")),
      8,
      Ok(ObjectFormat::Sha1),
    )
  );
  assert_eq!(
    written(|out| advertisement.write(out)),
    DOCUMENT_ADVERTISEMENT
  );
  let request = read_request("the document's requests", DOCUMENT_REQUESTS);
  let (lines, pack) = read_negotiation("the document's answers", DOCUMENT_ANSWERS);
  assert_eq!((request.len(), lines.len(), pack.len()), (10, 5, 0));
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
  let read: Vec<Capability<'_>> = advertisement.capabilities.iter().collect();
  assert_eq!(
    (&read[..], advertisement.capabilities.object_format()),
    (&made[..], Ok(ObjectFormat::Sha1))
  );
  assert_eq!(written(|out| advertisement.write(out)), bytes);
  let remade = Advertisement {
    capabilities: Capabilities::listed(&made),
    ..advertisement.clone()
  };
  assert_eq!(
    (&remade, written(|out| remade.write(out))),
    (&advertisement, bytes.clone())
  );
  let want = format!("want {tip} multi_ack_detailed teleport=yes side-band-64k\n");
  let line = ClientLine::read(want.as_bytes(), ObjectFormat::Sha1).unwrap();
  let made = [
    Capability {
      name: b"multi_ack_detailed",
      value: None,
    },
    Capability {
      name: b"teleport",
      value: Some(b"yes"),
    },
    Capability {
      name: b"side-band-64k",
      value: None,
    },
  ];
  let expected = ClientLine::Want {
    id: tip,
    capabilities: Some(Capabilities::listed(&made)),
  };
  assert_eq!(
    (line, written(|out| line.write(out))),
    (expected, packet(want.as_bytes()))
  );
}

/// A-604: do read each line with and without the LF git's writer ends it with, and its object name in upper case.
/// Expect: each read the same either way, as git's readers read it, and written back as git writes it: `want`, `have`,
/// `done`, `ACK` and `NAK` with LF; the request's `shallow`, `deepen` and `deepen-since` without.
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
      (
        ClientLine::read(other.as_bytes(), ObjectFormat::Sha1),
        written(|out| line.write(out))
      ),
      (Ok(line), packet(as_git_writes.as_bytes()))
    );
  }
  for (as_git_writes, other) in [
    ("NAK\n".to_owned(), "NAK".to_owned()),
    (format!("ACK {tip} ready\n"), format!("ACK {upper} ready")),
  ] {
    let line = ServerLine::read(as_git_writes.as_bytes(), ObjectFormat::Sha1).unwrap();
    assert_eq!(
      (
        ServerLine::read(other.as_bytes(), ObjectFormat::Sha1),
        written(|out| line.write(out))
      ),
      (Ok(line), packet(as_git_writes.as_bytes()))
    );
  }
}

/// A-604, refused by kind: do read client lines each malformed one way. Expect: each refused with its kind and the
/// byte where it stopped, none read as another line.
#[test]
fn malformed_client_lines_are_refused_by_kind_where_they_stop() {
  let tip = recorded("refs/heads/main").to_string();
  let short = tip.get(..39).unwrap();
  let id = |at, refusal| LineRefusal::Id { at, refusal };
  let cases = [
    (
      format!("want {short}"),
      id(5, HexRefusal::Length { len: 39 }),
    ),
    (
      format!("want {short}g"),
      id(5, HexRefusal::NotHex { at: 39 }),
    ),
    (
      format!("want {tip}\0"),
      id(5, HexRefusal::Length { len: 41 }),
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
    ("have ".to_owned(), id(5, HexRefusal::Length { len: 0 })),
    (
      format!("shallow {tip}x"),
      id(8, HexRefusal::Length { len: 41 }),
    ),
    (format!("wants {tip}"), LineRefusal::Unknown),
    ("done ".to_owned(), LineRefusal::Form { at: 4 }),
    ("done\r".to_owned(), LineRefusal::Unknown),
    (String::new(), LineRefusal::Unknown),
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
    Err(id(5, HexRefusal::Length { len: 40 }))
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

/// Checks that the advertisement `lines` make, then a flush, is refused with `refusal`.
fn refused_advertisement(lines: &[&[u8]], refusal: Refusal) {
  let bytes = stream(lines);
  assert_eq!(
    Advertisement::decode(&bytes, &mut room()),
    Err(refusal),
    "{:?}",
    bytes.escape_ascii().to_string()
  );
}

/// A-604, refused by kind: do decode advertisements whose lines break the grammar one way each. Expect: each refused
/// as a line where its packet begins: a first line with no NUL or no capabilities, an `object-format` git does not
/// know, a name too short for SHA-256, a ref name with a space, a shallow line's name too long.
#[test]
fn advertisement_lines_that_break_the_grammar_are_refused_by_kind() {
  let (tip, two) = (recorded("refs/heads/main"), commit("two"));
  let first = format!("{tip} HEAD\0multi_ack\n");
  let line = |lines: &[&[u8]], index, refusal| {
    refused_advertisement(
      lines,
      Refusal::Line {
        at: packet_at(lines, index),
        refusal,
      },
    );
  };
  line(
    &[format!("{tip} HEAD\n").as_bytes()],
    0,
    LineRefusal::Form { at: 45 },
  );
  line(
    &[format!("{tip} HEAD\0\n").as_bytes()],
    0,
    LineRefusal::Capability { at: 46 },
  );
  line(
    &[format!("{tip} HEAD\0multi_ack object-format=sha3\n").as_bytes()],
    0,
    LineRefusal::Format { at: 56 },
  );
  line(
    &[format!("{tip} HEAD\0object-format=sha256\n").as_bytes()],
    0,
    LineRefusal::Id {
      at: 0,
      refusal: HexRefusal::Length { len: 40 },
    },
  );
  line(
    &[
      first.as_bytes(),
      format!("{tip} refs/heads/a b\n").as_bytes(),
    ],
    1,
    LineRefusal::Name { at: 53 },
  );
  line(
    &[first.as_bytes(), format!("shallow {two}x\n").as_bytes()],
    1,
    LineRefusal::Id {
      at: 8,
      refusal: HexRefusal::Length { len: 41 },
    },
  );
}

/// A-640 (A-615's finding: this module read a ref's name as a word): do decode advertisements naming a ref git's
/// `check_refname_format` refuses (`..`, a name outside `refs/`, `.lock` at the end, `@{`), and write each. Expect:
/// each refused at its name, read or written; `HEAD` and `receive-pack`'s `.have` taken.
#[test]
fn an_advertised_ref_name_is_held_to_gits_ref_name_rule() {
  let (tip, two) = (recorded("refs/heads/main"), commit("two"));
  let first = format!("{tip} HEAD\0multi_ack\n");
  for name in [
    "refs/heads/a..b",
    "main",
    "refs/heads/x.lock",
    "refs/heads/a@{1}",
  ] {
    let second = format!("{tip} {name}\n");
    let lines: [&[u8]; 2] = [first.as_bytes(), second.as_bytes()];
    refused_advertisement(
      &lines,
      Refusal::Line {
        at: packet_at(&lines, 1),
        refusal: LineRefusal::Name { at: 41 },
      },
    );
    let written = Advertisement {
      version: Version::V0,
      capabilities: Capabilities::listed(&[]),
      refs: vec![AdvertisedRef {
        id: tip,
        name: name.as_bytes(),
        peeled: None,
      }],
      shallows: Vec::new(),
    }
    .write(&mut Vec::new());
    assert_eq!(
      written,
      Err(WriteRefusal::Line(LineRefusal::Name { at: 41 })),
      "{name}"
    );
  }
  let alternate = format!("{two} .have\n");
  let bytes = stream(&[first.as_bytes(), alternate.as_bytes()]);
  let (advertisement, _) = decoded(&bytes);
  let names: Vec<&[u8]> = advertisement
    .refs
    .iter()
    .map(|reference| reference.name)
    .collect();
  assert_eq!(names, [b"HEAD".as_slice(), b".have"]);
}

/// A-604, refused by kind: do decode advertisements whose order breaks the grammar one way each. Expect: each refused as
/// unexpected where its packet begins: a peeled line after no ref or after another ref, a ref after the shallow lines,
/// `capabilities^{}` with a name not null, a ref after it, it again on a later line, a peeled first line.
#[test]
fn advertisements_out_of_order_are_refused_where_the_line_stands() {
  let (tip, two) = (recorded("refs/heads/main"), commit("two"));
  let first = format!("{tip} HEAD\0multi_ack\n");
  let null = "0".repeat(40);
  let unexpected = |lines: &[&[u8]], index| {
    refused_advertisement(
      lines,
      Refusal::Unexpected {
        at: packet_at(lines, index),
      },
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
}

/// A-604, refused by kind: do decode what is no advertisement: a bare flush, a version other than 1, `ERR` in the first
/// line's place and after it, a delimiter, bytes that are no pkt-line. Expect: each refused by its kind where its
/// packet begins.
#[test]
fn what_is_no_advertisement_is_refused_by_kind() {
  let first = format!("{} HEAD\0multi_ack\n", recorded("refs/heads/main"));
  let after_first = to_u64(first.len() + 4);
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
      Refusal::Unexpected { at: after_first },
    ),
    (
      stream(&[first.as_bytes(), b"ERR gone\n"]),
      Refusal::ServerError { at: after_first },
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
  let (tip, two) = (recorded("refs/heads/main"), commit("two"));
  let unshallow = format!("unshallow {tip}");
  let shallow = format!("shallow {two}");
  let line = |refusal| Refusal::Line { at: 0, refusal };
  for (bytes, refusal) in [
    (
      stream(&[unshallow.as_bytes(), shallow.as_bytes()]),
      Refusal::Unexpected {
        at: to_u64(unshallow.len() + 4),
      },
    ),
    (stream(&[b"NAK\n"]), line(LineRefusal::Unknown)),
    (
      stream(&[format!("{shallow}x").as_bytes()]),
      line(LineRefusal::Id {
        at: 8,
        refusal: HexRefusal::Length { len: 41 },
      }),
    ),
    (stream(&[b"unshallow"]), line(LineRefusal::Form { at: 9 })),
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

/// A-604: do write lines with a part a reader would refuse, and a line past a packet's data. Expect: each refused by
/// kind, nothing written.
#[test]
fn a_line_writer_refuses_what_a_reader_would_and_writes_nothing() {
  let tip = recorded("refs/heads/main");
  let long = vec![b'x'; MAX_DATA];
  let bad_name = [Capability {
    name: b"Bad",
    value: None,
  }];
  let equals_in_name = [Capability {
    name: b"a=b",
    value: None,
  }];
  let want = |capabilities| ClientLine::Want {
    id: tip,
    capabilities: Some(capabilities),
  };
  let refused = |refusal| (Err(WriteRefusal::Line(refusal)), 0);
  for (line, expected) in [
    (
      ClientLine::Deepen(0),
      refused(LineRefusal::Number { at: 7 }),
    ),
    (
      ClientLine::Deepen(1 << 31),
      refused(LineRefusal::Number { at: 7 }),
    ),
    (
      ClientLine::DeepenSince(u64::MAX),
      refused(LineRefusal::Number { at: 13 }),
    ),
    (
      ClientLine::DeepenNot(b"a b"),
      refused(LineRefusal::Name { at: 12 }),
    ),
    (
      ClientLine::Filter(b""),
      refused(LineRefusal::Text { at: 7 }),
    ),
    (
      ClientLine::Error(b"a\nb"),
      refused(LineRefusal::Text { at: 5 }),
    ),
    (
      want(Capabilities::listed(&[])),
      refused(LineRefusal::Capability { at: 46 }),
    ),
    (
      want(Capabilities::listed(&bad_name)),
      refused(LineRefusal::Capability { at: 46 }),
    ),
    (
      want(Capabilities::listed(&equals_in_name)),
      refused(LineRefusal::Capability { at: 47 }),
    ),
    (
      ClientLine::Filter(&long),
      (
        Err(WriteRefusal::TooLong {
          len: to_u64(MAX_DATA + 7),
        }),
        0,
      ),
    ),
  ] {
    assert_eq!(attempt(|out| line.write(out)), expected, "{line:?}");
  }
  assert_eq!(
    attempt(|out| ServerLine::Error(b"a\0").write(out)),
    refused(LineRefusal::Text { at: 5 })
  );
}

/// A-604: do write advertisements and a shallow-update each with one part a reader would refuse, the bad part after good
/// lines. Expect: each refused by kind, and nothing written: no line before the bad one.
#[test]
fn a_message_writer_checks_the_whole_message_before_its_first_byte() {
  let tip = recorded("refs/heads/main");
  let wide = ObjectId::from_hex(ObjectFormat::Sha256, SHA256_TIP.as_bytes()).unwrap();
  let (advertisement, _) = decoded(file("advertisement-v0.bin"));
  let with_ref = |index: usize, name: &'static [u8], peeled| {
    let mut changed = advertisement.clone();
    changed.refs[index] = AdvertisedRef {
      id: tip,
      name,
      peeled,
    };
    changed
  };
  let unknown_format = [Capability {
    name: b"object-format",
    value: Some(b"sha3"),
  }];
  for (changed, refusal) in [
    (
      with_ref(3, b"refs/tags/v2^{}", None),
      WriteRefusal::Line(LineRefusal::Name { at: 41 }),
    ),
    (
      with_ref(2, b"a b", None),
      WriteRefusal::Line(LineRefusal::Name { at: 42 }),
    ),
    (
      with_ref(3, b"refs/tags/v1", Some(wide)),
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
    assert_eq!(attempt(|out| changed.write(out)), (Err(refusal), 0));
  }
  let mixed = ShallowUpdate {
    shallow: vec![tip],
    unshallow: vec![wide],
  };
  assert_eq!(
    attempt(|out| mixed.write(out)),
    (Err(WriteRefusal::Format), 0)
  );
}

/// A-604, bounds: do decode the v0 advertisement (four refs, one peeled line folded in) and the deepened exchange's
/// shallow-update (one `shallow`, one `unshallow`) under an admission one byte short of their lists, then under exactly
/// enough. Expect: refused `PastAdmission` with the admission untouched, then decoded with it spent to zero.
#[test]
fn a_decode_past_its_admission_is_refused_before_its_lists_are_made() {
  let bytes = file("advertisement-v0.bin");
  let lists = to_u64(4 * core::mem::size_of::<AdvertisedRef<'_>>());
  let (mut short, mut exact) = (Admission::new(lists - 1), Admission::new(lists));
  let past = Refusal::PastAdmission(PastAdmission {
    asked: lists,
    left: lists - 1,
  });
  assert_eq!(
    (
      Advertisement::decode(bytes, &mut short).map(|whole| whole.is_some()),
      short.left(),
      Advertisement::decode(bytes, &mut exact).map(|whole| whole.is_some()),
      exact.left(),
    ),
    (Err(past), lists - 1, Ok(true), 0)
  );
  let response = file("deepen-v0.response.bin");
  let update = &response[decoded(response).1..];
  let lists = to_u64(2 * core::mem::size_of::<ObjectId>());
  let (mut short, mut exact) = (Admission::new(lists - 1), Admission::new(lists));
  let past = Refusal::PastAdmission(PastAdmission {
    asked: lists,
    left: lists - 1,
  });
  assert_eq!(
    (
      ShallowUpdate::decode(update, ObjectFormat::Sha1, &mut short).map(|whole| whole.is_some()),
      short.left(),
      ShallowUpdate::decode(update, ObjectFormat::Sha1, &mut exact).map(|whole| whole.is_some()),
      exact.left(),
    ),
    (Err(past), lists - 1, Ok(true), 0)
  );
}

/// Every shallow-update stock git wrote, alone: the bytes after the advertisement in each exchange that asked for
/// depth, and the stateless `unshallow` answer's first message.
fn shallow_updates() -> Vec<(&'static str, &'static [u8])> {
  let mut updates: Vec<(&str, &[u8])> = ["depth-v0", "deepen-v0", "since-v0", "exclude-v0"]
    .into_iter()
    .map(|name| {
      let response = file(&format!("{name}.response.bin"));
      (name, &response[decoded(response).1..])
    })
    .collect();
  updates.push(("unshallow", file("unshallow.response.bin")));
  updates
    .into_iter()
    .map(|(name, bytes)| (name, &bytes[..decoded_update(bytes).1]))
    .collect()
}

/// The data of every line of an answer between its opening messages (its advertisement when `advertised`, then its
/// shallow-update when it has one) and its pack.
fn answer_lines(response: &[u8], advertised: bool) -> Vec<&[u8]> {
  let mut rest = response;
  if advertised {
    rest = &rest[decoded(rest).1..];
  }
  if let Ok(Some((_, took))) = ShallowUpdate::decode(rest, ObjectFormat::Sha1, &mut room()) {
    rest = &rest[took..];
  }
  packets(rest)
    .into_iter()
    .map_while(|received| match received {
      Packet::Data(data) if !on_a_band(data) => Some(data),
      _ => None,
    })
    .collect()
}

/// Every line stock git wrote in a request or a negotiation, each with whether it is a client's.
fn every_line() -> Vec<(bool, &'static [u8])> {
  let mut lines = Vec::new();
  for (name, advertised) in EXCHANGES {
    let request = packets(file(&format!("{name}.request.bin")));
    lines.extend(request.into_iter().filter_map(|received| match received {
      Packet::Data(data) => Some((true, data)),
      _ => None,
    }));
    let response = file(&format!("{name}.response.bin"));
    lines.extend(
      answer_lines(response, advertised)
        .into_iter()
        .map(|data| (false, data)),
    );
  }
  lines
}

/// The data a line writes, or `None` when it is refused, read as a client's line or a server's.
fn reread(client: bool, data: &[u8]) -> Option<Vec<u8>> {
  let mut out = Vec::new();
  if client {
    let line = ClientLine::read(data, ObjectFormat::Sha1).ok()?;
    line.write(&mut out).ok()?;
  } else {
    let line = ServerLine::read(data, ObjectFormat::Sha1).ok()?;
    line.write(&mut out).ok()?;
  }
  Some(data_of(&out).to_vec())
}

/// A-604, hostile input: do decode every cut of every advertisement and every shallow-update stock git wrote. Expect:
/// every message cut short answers `None` (not yet whole), never a refusal or a message.
#[test]
fn every_cut_of_a_message_waits_for_the_rest() {
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
}

/// A-604, hostile input: do read every cut of every line stock git wrote in a request or a negotiation. Expect: each
/// cut refused, or read as exactly the bytes it kept and written back to them (its LF aside), never as more.
#[test]
fn every_cut_of_a_line_reads_as_its_bytes_say() {
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
  assert!(read > 0, "some cuts are lines of their own");
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

impl Reached {
  /// Counts a decode of a flipped message `len` bytes long, checking a refusal's place is inside it.
  fn count<T>(&mut self, decoded: &Result<Option<T>, Refusal>, len: usize) {
    let at = match decoded {
      Ok(Some(_)) => {
        self.read += 1;
        return;
      }
      Ok(None) => {
        self.waiting += 1;
        return;
      }
      Err(Refusal::Packet(_)) => {
        self.packet += 1;
        return;
      }
      Err(Refusal::Line { at, .. }) => {
        self.line += 1;
        at
      }
      Err(Refusal::Unexpected { at } | Refusal::Version { at } | Refusal::ServerError { at }) => {
        self.unexpected += 1;
        at
      }
      Err(Refusal::PastAdmission(past)) => panic!("{past:?} past a room no message exceeds"),
    };
    assert!(usize::try_from(*at).unwrap() < len);
  }

  /// Whether the flips reached every outcome.
  fn everything(&self) -> bool {
    [
      self.read,
      self.waiting,
      self.packet,
      self.line,
      self.unexpected,
    ]
    .iter()
    .all(|count| *count > 0)
  }
}

/// A-604, hostile input: do flip every bit of every byte of every advertisement stock git wrote, and decode each.
/// Expect: no panic; every refusal typed, at a place inside the bytes; and what still decodes reads as its bytes say:
/// written back to them, or to the original where the flip made an object name's digit upper case, or, where the flip
/// took the LF off a line, to them with that LF again.
#[test]
fn every_bit_flip_of_an_advertisement_is_refused_by_kind_or_read_as_its_bytes_say() {
  let mut reached = Reached::default();
  for name in ADVERTISEMENTS {
    let bytes = file(name);
    for index in 0..bytes.len() {
      for bit in 0..8 {
        let mut flipped = bytes.to_vec();
        flipped[index] ^= 1 << bit;
        let decoded_flip = Advertisement::decode(&flipped, &mut room());
        reached.count(&decoded_flip, flipped.len());
        if let Ok(Some((advertisement, took))) = decoded_flip {
          let again = written(|out| advertisement.write(out));
          let as_read = again == flipped[..took]
            || (upper_case_flip(bytes, &flipped, index) && again == bytes[..took])
            || (bytes.get(index) == Some(&b'\n')
              && again.len() == took + 1
              && decoded(&again).0 == advertisement);
          assert!(as_read, "{name} {index}.{bit}");
        }
      }
    }
  }
  assert!(reached.everything(), "{reached:?}");
}

/// A-604, hostile input: do flip every bit of every byte of every shallow-update stock git wrote, and decode each.
/// Expect: no panic; every refusal typed, at a place inside the bytes; and what still decodes written back to its
/// bytes, or to the original where the flip made an object name's digit upper case.
#[test]
fn every_bit_flip_of_a_shallow_update_is_refused_by_kind_or_read_as_its_bytes_say() {
  let mut reached = Reached::default();
  for (name, bytes) in shallow_updates() {
    for index in 0..bytes.len() {
      for bit in 0..8 {
        let mut flipped = bytes.to_vec();
        flipped[index] ^= 1 << bit;
        let decoded_flip = ShallowUpdate::decode(&flipped, ObjectFormat::Sha1, &mut room());
        reached.count(&decoded_flip, flipped.len());
        if let Ok(Some((update, took))) = decoded_flip {
          let again = written(|out| update.write(out));
          let as_read = again == flipped[..took]
            || (upper_case_flip(bytes, &flipped, index) && again == bytes[..took]);
          assert!(as_read, "{name} {index}.{bit}");
        }
      }
    }
  }
  assert!(
    reached.read > 0 && reached.waiting > 0 && reached.line > 0,
    "{reached:?}"
  );
}

/// A-604, hostile input: do flip every bit of every byte of every line stock git wrote in a request or a negotiation,
/// and read each. Expect: no panic; and what still reads written back to its bytes (its LF aside), or to the original
/// where the flip made an object name's digit upper case.
#[test]
fn every_bit_flip_of_a_line_is_refused_or_read_as_its_bytes_say() {
  let mut read = 0;
  for (client, data) in every_line() {
    for index in 0..data.len() {
      for bit in 0..8 {
        let mut flipped = data.to_vec();
        flipped[index] ^= 1 << bit;
        if let Some(again) = reread(client, &flipped) {
          let as_read = chomped(&again) == chomped(&flipped)
            || (upper_case_flip(data, &flipped, index) && chomped(&again) == chomped(data));
          assert!(as_read, "{:?} {index}.{bit}", data.escape_ascii());
          read += 1;
        }
      }
    }
  }
  assert!(read > 0, "some flips are lines of their own");
}

/// A capability's parts, owned.
type OwnedCapability = (Vec<u8>, Option<Vec<u8>>);

/// A ref's parts, owned: its object, its name, the object it peels to.
type OwnedRef = (ObjectId, Vec<u8>, Option<ObjectId>);

/// An advertisement's parts, owned.
type OwnedAdvertisement = (Version, Vec<OwnedCapability>, Vec<OwnedRef>, Vec<ObjectId>);

/// Shape: the bytes a generated capability's name takes, as git's names do.
const NAME_BYTES: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789-_";

/// A capability a generated list may hold: a name in the grammar, and maybe a value of bytes a ref name may hold.
fn any_capability() -> impl Strategy<Value = OwnedCapability> {
  let name_byte = (0..NAME_BYTES.len()).prop_map(|at| NAME_BYTES[at]);
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
  any::<bool>().prop_map(|sha256| {
    if sha256 {
      ObjectFormat::Sha256
    } else {
      ObjectFormat::Sha1
    }
  })
}

/// A client line's parts, owned, from which a test borrows the line.
#[derive(Clone, Debug)]
enum OwnedClientLine {
  Want(ObjectId, Option<Vec<OwnedCapability>>),
  Have(ObjectId),
  Done,
  Shallow(ObjectId),
  Deepen(u32),
  DeepenSince(u64),
  DeepenNot(Vec<u8>),
  Filter(Vec<u8>),
  Error(Vec<u8>),
}

/// A client line's parts, before its kind is chosen: an id and maybe capabilities, a depth and a time, and three runs
/// of bytes (a ref name's, a filter's, an error's).
type ClientLineParts = (
  u8,
  (ObjectId, Option<Vec<OwnedCapability>>),
  (u32, u64),
  (Vec<u8>, Vec<u8>, Vec<u8>),
);

/// Shape: the kinds of client line a selector chooses among, one each.
const CLIENT_LINE_KINDS: u8 = 9;

/// Any client line a writer takes, in `format`: its kind chosen by a selector over parts made for every kind (no
/// `prop_oneof!`, whose strategy is reference-counted, R2).
fn any_client_line(format: ObjectFormat) -> impl Strategy<Value = OwnedClientLine> {
  let parts = (
    0..CLIENT_LINE_KINDS,
    (
      any_id(format),
      proptest::option::of(proptest::collection::vec(any_capability(), 1..6)),
    ),
    (1..=i32::MAX.unsigned_abs(), 0..u64::MAX),
    (
      proptest::collection::vec(any_word_byte(), 1..20),
      proptest::collection::vec(any_text_byte(), 1..20),
      proptest::collection::vec(any_text_byte(), 0..20),
    ),
  );
  parts.prop_map(client_line_of)
}

/// The client line `parts` makes, of the kind its selector names.
fn client_line_of(parts: ClientLineParts) -> OwnedClientLine {
  let (selector, (id, capabilities), (depth, since), (not, filter, error)) = parts;
  match selector {
    0 => OwnedClientLine::Want(id, capabilities),
    1 => OwnedClientLine::Have(id),
    2 => OwnedClientLine::Done,
    3 => OwnedClientLine::Shallow(id),
    4 => OwnedClientLine::Deepen(depth),
    5 => OwnedClientLine::DeepenSince(since),
    6 => OwnedClientLine::DeepenNot(not),
    7 => OwnedClientLine::Filter(filter),
    _ => OwnedClientLine::Error(error),
  }
}

/// The capabilities `owned` lists, borrowed.
fn borrowed(owned: &[OwnedCapability]) -> Vec<Capability<'_>> {
  owned
    .iter()
    .map(|(name, value)| Capability {
      name,
      value: value.as_deref(),
    })
    .collect()
}

/// The line `owned` describes, its capabilities borrowed from `capabilities`.
fn borrowed_line<'a>(
  owned: &'a OwnedClientLine,
  capabilities: &'a [Capability<'a>],
) -> ClientLine<'a> {
  match owned {
    OwnedClientLine::Want(id, list) => ClientLine::Want {
      id: *id,
      capabilities: list.as_ref().map(|_| Capabilities::listed(capabilities)),
    },
    OwnedClientLine::Have(id) => ClientLine::Have(*id),
    OwnedClientLine::Done => ClientLine::Done,
    OwnedClientLine::Shallow(id) => ClientLine::Shallow(*id),
    OwnedClientLine::Deepen(depth) => ClientLine::Deepen(*depth),
    OwnedClientLine::DeepenSince(time) => ClientLine::DeepenSince(*time),
    OwnedClientLine::DeepenNot(name) => ClientLine::DeepenNot(name),
    OwnedClientLine::Filter(spec) => ClientLine::Filter(spec),
    OwnedClientLine::Error(text) => ClientLine::Error(text),
  }
}

/// The format of a line's object name, SHA-1 for a line that has none.
fn line_format(line: &ClientLine<'_>) -> ObjectFormat {
  match line {
    ClientLine::Want { id, .. } | ClientLine::Have(id) | ClientLine::Shallow(id) => id.format(),
    _ => ObjectFormat::Sha1,
  }
}

/// A ref a generated advertisement may hold: a name its ref-name rule takes, maybe a peeled object.
fn any_ref(format: ObjectFormat) -> impl Strategy<Value = OwnedRef> {
  (
    any_id(format),
    any_ref_name(),
    proptest::option::of(any_id(format)),
  )
}

/// A name an advertised ref may have (A-640: held to git's ref-name rule): `HEAD`, or one to three components under
/// `refs/`, each of letters, digits, `-` and `_`.
fn any_ref_name() -> impl Strategy<Value = Vec<u8>> {
  let byte = prop_oneof![b'a'..=b'z', b'0'..=b'9', Just(b'-'), Just(b'_')];
  let component = proptest::collection::vec(byte, 1..8);
  prop_oneof![
    1 => Just(b"HEAD".to_vec()),
    8 => proptest::collection::vec(component, 1..4).prop_map(|components| {
      let mut name = b"refs".to_vec();
      for component in components {
        name.push(b'/');
        name.extend(component);
      }
      name
    }),
  ]
}

/// An advertisement's parts: its version, its capabilities with an `object-format` naming its format added last, its
/// refs and its shallow commits.
fn any_advertisement() -> impl Strategy<Value = OwnedAdvertisement> {
  any_format().prop_flat_map(|format| {
    (
      any::<bool>().prop_map(|v1| if v1 { Version::V1 } else { Version::V0 }),
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

/// Every status an `ACK` line may carry.
const ACK_STATUSES: [AckStatus; 4] = [
  AckStatus::Plain,
  AckStatus::Continue,
  AckStatus::Common,
  AckStatus::Ready,
];

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
    let line = borrowed_line(&owned, &capabilities);
    let bytes = written(|out| line.write(out));
    let data = data_of(&bytes);
    let ends_in_lf = matches!(line, ClientLine::Want { .. } | ClientLine::Have(_) | ClientLine::Done);
    prop_assert_eq!(
      (ClientLine::read(data, line_format(&line)), data.ends_with(b"\n")),
      (Ok(line), ends_in_lf)
    );
  }

  /// A-604: do write any server line, in either format, and read it back. Expect: read as the same line.
  #[test]
  fn any_server_line_written_reads_back_as_itself(
    id in any_format().prop_flat_map(any_id),
    status in (0..ACK_STATUSES.len()).prop_map(|at| ACK_STATUSES[at]),
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
      shallows,
    };
    let bytes = written(|out| advertisement.write(out));
    let no_refs: &[u8] = b"capabilities^{}\0";
    let names_no_refs = bytes.windows(no_refs.len()).any(|window| window == no_refs);
    prop_assert_eq!(
      (decoded(&bytes), names_no_refs),
      ((advertisement.clone(), bytes.len()), advertisement.refs.is_empty())
    );
    let format = unshallow.first().map_or(ObjectFormat::Sha1, ObjectId::format);
    let update = ShallowUpdate { shallow: Vec::new(), unshallow };
    let bytes = written(|out| update.write(out));
    prop_assert_eq!(
      ShallowUpdate::decode(&bytes, format, &mut room()),
      Ok(Some((update, bytes.len())))
    );
  }
}
