//! receive-pack's tests (A-620): eleven pushes stock git made as golden vectors, the request
//! reader, each command's checks, the report and the admission. `append` holds the tests of the
//! appends that move refs (A-642), `checks` those of the pushed objects' fsck and of hidden refs,
//! `session` those of one push over one connection, `works` those of part 2's judge over a store.

mod append;
mod checks;
mod session;
mod works;

use std::collections::BTreeSet;
use std::io::Write as _;

use flate2::Compression;
use flate2::write::ZlibEncoder;

use super::{
  Advertised, Command, Connected, Declined, HiddenRefs, Judge, NoneHidden, Outcome, Pushed,
  Received, Refusal, Reported, Repository, RequestReader, UNPACK_LIMIT, connectivity, message,
  receive, visible, write_advertisement, write_report,
};
use crate::entry::{self, Header, Representation};
use crate::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use crate::objects::commit::Commit;
use crate::pack;
use crate::pktline::{self, Packet};
use crate::sideband::{self, Band};
use crate::unpack::{self, NoIndex};
use crate::{Admission, PastAdmission};

/// One push stock git 2.50.1 made to stock `git receive-pack`, captured by
/// `fixtures/push/make-push-fixtures.sh`.
struct Scenario {
  /// Its name in the script.
  name: &'static str,
  /// What `git push` sent: its commands, then its pack.
  request: &'static [u8],
  /// What `git receive-pack` answered: its advertisement, then its report.
  response: &'static [u8],
  /// Every object the server held before the push, as one pack.
  before_pack: &'static [u8],
  /// The server's refs before the push, `id SP name` a line.
  before_refs: &'static str,
}

/// Includes the four files of a scenario.
macro_rules! scenario {
  ($name:literal) => {
    Scenario {
      name: $name,
      request: include_bytes!(concat!("../../../fixtures/push/", $name, ".request.bin")),
      response: include_bytes!(concat!("../../../fixtures/push/", $name, ".response.bin")),
      before_pack: include_bytes!(concat!("../../../fixtures/push/", $name, ".before.pack")),
      before_refs: include_str!(concat!("../../../fixtures/push/", $name, ".before.refs")),
    }
  };
}

/// Every scenario, in the order the script pushed them.
fn scenarios() -> [Scenario; 11] {
  [
    scenario!("create"),
    scenario!("update"),
    scenario!("tag"),
    scenario!("copy"),
    scenario!("side"),
    scenario!("delete"),
    scenario!("options"),
    scenario!("non-ff"),
    scenario!("atomic"),
    scenario!("hidden"),
    scenario!("fsck"),
  ]
}

/// The scenario the script named `name`.
fn scenario(name: &str) -> Scenario {
  scenarios()
    .into_iter()
    .find(|scenario| scenario.name == name)
    .unwrap()
}

/// What the captured server advertised: every capability git's defaults and the script's
/// `receive.advertisePushOptions` give, and stock git's agent.
const ADVERTISED: Advertised<'static> = Advertised {
  format: ObjectFormat::Sha1,
  atomic: true,
  ofs_delta: true,
  push_options: true,
  session_id: None,
  agent: b"git/2.50.1-Darwin",
};

/// The refs git's `receive.hideRefs` hides, read as git's `ref_is_hidden` (refs.c) reads a plain
/// pattern: the ref of its name and every ref beneath it.
struct HideRefs(&'static [&'static [u8]]);

impl HiddenRefs for HideRefs {
  fn is_hidden(&self, name: &[u8]) -> bool {
    self.0.iter().any(|pattern| {
      name
        .strip_prefix(*pattern)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(b"/"))
    })
  }
}

/// What the captured server hid from its hidden push on (the script's `receive.hideRefs`): no
/// earlier server holds a ref beneath it, so it is every scenario's.
const HIDDEN: HideRefs = HideRefs(&[b"refs/hidden"]);

/// `n` as a `u64`.
fn wide(n: usize) -> u64 {
  u64::try_from(n).unwrap()
}

/// An admission that holds anything, for reads whose charges a test does not look at.
fn unbounded() -> Admission {
  Admission::new(u64::MAX)
}

/// A clock one nanosecond on at each reading, so a quantum of none ends each turn after its first
/// unit of work.
struct Ticking(u64);

impl crate::store::learn::Clock for Ticking {
  fn now_ns(&mut self) -> u64 {
    self.0 = self.0.saturating_add(1);
    self.0
  }
}

/// Makes `session`'s pack's checks against `repository` a unit of work a turn until the push is
/// checked or refused (A-719): the turns taken, and the last turn's outcome.
fn checked_in_turns<H: HiddenRefs, R: Repository + ?Sized>(
  session: &mut super::Session<'_, H>,
  repository: &R,
) -> (usize, Result<bool, Refusal>) {
  let mut clock = Ticking(0);
  let mut turns = 0;
  while session.is_checking() {
    turns += 1;
    let quantum = crate::store::hook_view::Quantum {
      clock: &mut clock,
      ns: 0,
    };
    let turned = session.check_turn(repository, &mut unbounded(), quantum);
    if !matches!(turned, Ok(false)) {
      return (turns, turned);
    }
  }
  (turns, Ok(session.checked().is_some()))
}

/// A repository a test holds: each object by name.
#[derive(Default)]
struct Repo {
  objects: Vec<(ObjectId, Kind, Vec<u8>)>,
}

impl Repo {
  /// The objects of a pack the script wrote of the server.
  fn from_pack(bytes: &[u8]) -> Self {
    let unpacked =
      unpack::read_whole(bytes, ObjectFormat::Sha1, &NoIndex, &mut unbounded()).unwrap();
    let mut objects: Vec<(ObjectId, Kind, Vec<u8>)> = unpacked
      .objects
      .into_iter()
      .map(|object| (object.id, object.kind, object.content))
      .collect();
    objects.sort_by_key(|a| a.0);
    Self { objects }
  }

  /// The same, with `content` of `kind` held too: its name.
  fn with(&mut self, kind: Kind, content: &[u8]) -> ObjectId {
    let id = name_of(ObjectFormat::Sha1, kind, content).unwrap();
    self.objects.push((id, kind, content.to_vec()));
    self.objects.sort_by_key(|a| a.0);
    id
  }
}

impl Repository for Repo {
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])> {
    let at = self
      .objects
      .binary_search_by(|(known, _, _)| known.cmp(id))
      .ok()?;
    let (_, kind, content) = self.objects.get(at)?;
    Some((*kind, content.as_slice()))
  }
}

/// The refs of a scenario's server, as the advertisement names them.
fn refs(lines: &str) -> Vec<(ObjectId, Vec<u8>)> {
  lines
    .lines()
    .map(|line| {
      let (hex, name) = line.split_once(' ').unwrap();
      (
        ObjectId::from_hex(ObjectFormat::Sha1, hex.as_bytes()).unwrap(),
        name.as_bytes().to_vec(),
      )
    })
    .collect()
}

/// `refs` as the advertisement takes them.
fn named(refs: &[(ObjectId, Vec<u8>)]) -> Vec<(ObjectId, &[u8])> {
  refs
    .iter()
    .map(|(id, name)| (*id, name.as_slice()))
    .collect()
}

/// git's own verdicts on the captured server, which refused non-fast-forwards
/// (`receive.denyNonFastForwards`): an update of a branch whose old commit is not an ancestor of
/// the new is declined in git's words, with git's band-2 line.
struct GitPolicy<'r> {
  repository: &'r Repo,
  judged: Vec<Vec<u8>>,
}

impl GitPolicy<'_> {
  /// Whether `old` is `new` or one of its ancestors, walking commits pushed and held.
  fn reaches(&self, pushed: &Pushed<'_>, new: ObjectId, old: ObjectId) -> bool {
    let mut seen = BTreeSet::new();
    let mut next = vec![new];
    while let Some(id) = next.pop() {
      if id == old {
        return true;
      }
      if !seen.insert(id) {
        continue;
      }
      let content = match pushed.object(&id) {
        Some(object) => object.content.as_slice(),
        None => match self.repository.object(&id) {
          Some((_, content)) => content,
          None => continue,
        },
      };
      let commit = Commit::parse(content, &mut unbounded()).unwrap();
      next.extend(commit.parents(ObjectFormat::Sha1));
    }
    false
  }
}

impl Judge for GitPolicy<'_> {
  type Accepted = ();

  fn judge(&mut self, command: &Command, pushed: &Pushed<'_>) -> Result<(), Declined> {
    self.judged.push(command.name.clone());
    let update = !command.is_create() && !command.is_delete();
    if update
      && command.name.starts_with(b"refs/heads/")
      && !self.reaches(pushed, command.new, command.old)
    {
      let line = [
        b"denying non-fast-forward ",
        command.name.as_slice(),
        b" (you should pull first)",
      ]
      .concat();
      return Err(Declined {
        reason: b"non-fast-forward".to_vec(),
        hook: Vec::new(),
        message: Some(message(&line)),
      });
    }
    Ok(())
  }

  fn append(&mut self, _command: &Command, (): ()) -> Result<Vec<Reported>, Declined> {
    Ok(Vec::new())
  }

  fn append_atomic(
    &mut self,
    _accepted: Vec<(&Command, ())>,
  ) -> Result<Vec<Vec<Reported>>, Declined> {
    Ok(Vec::new())
  }
}

/// git's policy over `repository`, nothing judged yet.
fn git_policy(repository: &Repo) -> GitPolicy<'_> {
  GitPolicy {
    repository,
    judged: Vec::new(),
  }
}

/// `scenario`'s push received against its server, judged by git's policy, its hidden refs hidden.
fn received(scenario: &Scenario, repository: &Repo) -> Received {
  receive(
    scenario.request,
    &ADVERTISED,
    repository,
    &HIDDEN,
    &mut git_policy(repository),
    &mut unbounded(),
  )
  .unwrap()
}

/// The advertisement written for `scenario`'s server, its hidden refs left out.
fn advertisement(scenario: &Scenario) -> Vec<u8> {
  let refs = refs(scenario.before_refs);
  let mut out = Vec::new();
  write_advertisement(&visible(&named(&refs), &HIDDEN), &ADVERTISED, &mut out).unwrap();
  out
}

/// The answer written to `received`.
fn answer_to(received: &Received) -> Vec<u8> {
  let mut answer = Vec::new();
  write_report(received, &mut answer).unwrap();
  answer
}

/// A multiplexed answer's band 1 and band 2, each its packets' bytes joined, once the answer is
/// seen to end with its own flush and to hold nothing else.
fn bands(answer: &[u8]) -> (Vec<u8>, Vec<u8>) {
  let packets = pktline::decode(answer, &mut unbounded()).unwrap();
  let (last, multiplexed) = packets.split_last().unwrap();
  assert_eq!(*last, Packet::Flush);
  let mut report = Vec::new();
  let mut text = Vec::new();
  for packet in multiplexed {
    let Packet::Data(data) = packet else {
      panic!("{packet:?} inside an answer");
    };
    match sideband::demux(data).unwrap() {
      (Band::Data, bytes) => report.extend_from_slice(bytes),
      (Band::Progress, bytes) => text.extend_from_slice(bytes),
      (Band::Error, bytes) => panic!("band 3: {}", String::from_utf8_lossy(bytes)),
    }
  }
  (report, text)
}

/// Asserts that `ours` answers as `gits` does but for the prose git writes after an fsck finding's
/// id: band 1 byte for byte; band 2 line for line, each of git's lines ours, or ours, `: ` and
/// git's prose.
fn assert_agrees_but_for_prose(ours: &[u8], gits: &[u8], name: &str) {
  let (our_report, our_text) = bands(ours);
  let (git_report, git_text) = bands(gits);
  assert_eq!(our_report, git_report, "{name}: band 1");
  let our_lines: Vec<&[u8]> = our_text.split(|byte| *byte == b'\n').collect();
  let git_lines: Vec<&[u8]> = git_text.split(|byte| *byte == b'\n').collect();
  assert_eq!(our_lines.len(), git_lines.len(), "{name}: band 2's lines");
  let agree = our_lines.iter().zip(&git_lines).all(|(ours, gits)| {
    gits
      .strip_prefix(*ours)
      .is_some_and(|prose| prose.is_empty() || prose.starts_with(b": "))
  });
  let shown = |text: &[u8]| String::from_utf8_lossy(text).into_owned();
  assert!(
    agree,
    "{name}: {} against {}",
    shown(&our_text),
    shown(&git_text)
  );
}

/// A-620 (§4.17.2 piece 6; golden vectors: eleven pushes stock git 2.50.1 made to stock `git
/// receive-pack`, captured by `fixtures/push/make-push-fixtures.sh`). Do: write the advertisement
/// of each scenario's server from its refs, `refs/hidden` hidden as the script hid it; then
/// receive its request against a repository holding the server's objects, judged by the server's
/// own policy (it refused non-fast-forwards), and write the answer. Expect: the advertisement and
/// the answer, together, the very bytes `git receive-pack` wrote: the capabilities in git's order
/// (`capabilities^{}` for the empty repository), no hidden ref, band 2 before band 1, `unpack ok`
/// or git's words, `ok` or git's `ng` reasons (non-fast-forward, atomic push failure, deny updating
/// a hidden ref, unpacker error), the flushes. One answer differs, the fsck push's, and only in the
/// prose git writes on band 2 after its finding's id.
#[test]
fn every_push_stock_git_made_is_answered_with_the_bytes_git_answered() {
  let mut inexact = Vec::new();
  for scenario in scenarios() {
    let repository = Repo::from_pack(scenario.before_pack);
    let advertised = advertisement(&scenario);
    let (head, tail) = scenario.response.split_at(advertised.len());
    assert_eq!(advertised, head, "{}: the advertisement", scenario.name);
    let answer = answer_to(&received(&scenario, &repository));
    if answer != tail {
      assert_agrees_but_for_prose(&answer, tail, scenario.name);
      inexact.push(scenario.name);
    }
  }
  assert_eq!(inexact, ["fsck"]);
}

/// Asserts that `scenario`'s request asked what stock git asks: report-status-v2, side-band-64k
/// and quiet, its agent, `atomic` in the atomic push alone, and the push options `ci.skip` and
/// `note=two words` in the options push alone.
fn assert_asked_as_git_asks(scenario: &Scenario, received: &Received) {
  let request = &received.request;
  let requested = &request.requested;
  let name = scenario.name;
  assert!(
    requested.report_status_v2 && requested.side_band_64k && requested.quiet,
    "{name}"
  );
  assert_eq!(requested.agent.as_deref(), Some(&b"git/2.50.1-Darwin"[..]));
  assert_eq!(requested.atomic, name == "atomic", "{name}");
  let options: Vec<&[u8]> = request.options.iter().map(Vec::as_slice).collect();
  let expected: &[&[u8]] = if name == "options" {
    &[b"ci.skip", b"note=two words"]
  } else {
    &[]
  };
  assert_eq!(options, expected, "{name}");
}

/// A-620 (golden vectors, read). Do: read each captured request. Expect: the commands,
/// capabilities and options stock git sent (report-status-v2, side-band-64k and quiet asked every
/// time, its agent, `atomic` in the atomic push alone, the push options in the options push alone);
/// a pack after every request but the delete's; one outcome a command.
#[test]
fn every_request_stock_git_sent_reads_as_git_wrote_it() {
  for scenario in scenarios() {
    let repository = Repo::from_pack(scenario.before_pack);
    let received = received(&scenario, &repository);
    assert_asked_as_git_asks(&scenario, &received);
    assert_eq!(
      received.unpacked.is_some(),
      scenario.name != "delete",
      "{}",
      scenario.name
    );
    assert_eq!(received.outcomes.len(), received.request.commands.len());
  }
}

/// A-620 (thin packs: a base the server holds). Do: receive the pushes whose packs stock git made
/// thin (update, options, the non-fast-forward and the atomic push), against the server's objects.
/// Expect: each pack read, its delta on the text blob resolved one deep against the server's own
/// copy, a base the pack does not hold.
#[test]
fn a_thin_pack_resolves_its_deltas_against_the_repository() {
  let thin = ["update", "options", "non-ff", "atomic"];
  for scenario in scenarios()
    .into_iter()
    .filter(|scenario| thin.contains(&scenario.name))
  {
    let repository = Repo::from_pack(scenario.before_pack);
    let received = received(&scenario, &repository);
    let unpacked = received.unpacked.as_ref().unwrap().as_ref().unwrap();
    let outside: Vec<_> = unpacked
      .objects
      .iter()
      .filter_map(|object| object.chain)
      .filter(|chain| repository.holds(&chain.base))
      .collect();
    assert_eq!(outside.len(), 1, "{}", scenario.name);
    assert_eq!(outside[0].depth, 1, "{}", scenario.name);
  }
}

/// The bytes of `request` its request takes, before its pack.
fn request_len(request: &[u8]) -> usize {
  let mut reader = RequestReader::new(&ADVERTISED);
  reader.read(request, &mut unbounded()).unwrap()
}

/// A-620 (streaming). Do: feed each captured request to a reader a byte at a time. Expect: the
/// same request as read whole, the reader taking exactly the request's bytes and none of the
/// pack's.
#[test]
fn a_request_read_a_byte_at_a_time_is_the_same_and_stops_where_the_pack_begins() {
  for scenario in scenarios() {
    let took = request_len(scenario.request);
    let mut reader = RequestReader::new(&ADVERTISED);
    let mut fed = 0;
    for byte in scenario.request {
      fed += reader
        .read(core::slice::from_ref(byte), &mut unbounded())
        .unwrap();
      if reader.is_whole() {
        break;
      }
    }
    assert_eq!(fed, took, "{}", scenario.name);
    let whole = received(&scenario, &Repo::from_pack(scenario.before_pack)).request;
    assert_eq!(reader.request().unwrap(), whole, "{}", scenario.name);
    let pack = &scenario.request[took..];
    assert!(
      pack.is_empty() || pack.starts_with(b"PACK"),
      "{}",
      scenario.name
    );
  }
}

/// git's words for a short pack cut `into` bytes in: `eof before pack header was fully read`
/// within its header; after it, the program git runs for the objects its header counts
/// (`unpack-objects` below its `unpack_limit`, `index-pack` at or above it) and its abnormal exit.
fn short_pack_words(pack: &[u8], into: usize) -> &'static [u8] {
  if wide(into) < entry::PACK_HEADER_LEN {
    return b"eof before pack header was fully read";
  }
  let objects = u32::from_be_bytes(pack[8..12].try_into().unwrap());
  if objects < UNPACK_LIMIT {
    b"unpack-objects abnormal exit"
  } else {
    b"index-pack abnormal exit"
  }
}

/// Asserts that `received` answers a push whose pack (`pack`, whole) was cut `into` bytes in as git
/// answers a short pack: every command `unpacker error`, the unpack status git's words.
fn assert_short_pack(received: &Received, pack: &[u8], into: usize, label: &str) {
  let refusal = received.unpacked.as_ref().unwrap().as_ref().unwrap_err();
  let words = super::unpack_words(refusal);
  assert_eq!(words, short_pack_words(pack, into), "{label}");
  let all_unpacker = received
    .outcomes
    .iter()
    .all(|outcome| refused_for(outcome, b"unpacker error"));
  assert!(all_unpacker, "{label}");
}

/// A-620 (hostile input: every cut). Do: receive every cut of each captured push. Expect: a cut
/// inside the request refused as `Truncated` at the cut; a cut inside the pack answered as git
/// answers a short pack, every command `unpacker error`, the unpack status git's words (`eof before
/// pack header was fully read` within the pack's header, `unpack-objects abnormal exit` after it,
/// every captured pack counting fewer objects than git's `unpack_limit`); nothing judged.
#[test]
fn every_cut_of_a_push_is_refused_or_answered_as_a_short_pack() {
  for scenario in scenarios() {
    let repository = Repo::from_pack(scenario.before_pack);
    let request_len = request_len(scenario.request);
    for cut in 0..scenario.request.len() {
      let label = format!("{} cut at {cut}", scenario.name);
      let mut policy = git_policy(&repository);
      let bytes = &scenario.request[..cut];
      let result = receive(
        bytes,
        &ADVERTISED,
        &repository,
        &HIDDEN,
        &mut policy,
        &mut unbounded(),
      );
      match cut.checked_sub(request_len) {
        None => assert_eq!(result, Err(Refusal::Truncated { at: wide(cut) }), "{label}"),
        Some(into) => {
          let pack = &scenario.request[request_len..];
          assert_short_pack(&result.unwrap(), pack, into, &label);
        }
      }
      assert!(policy.judged.is_empty(), "{label}: nothing judged");
    }
  }
}

/// The name of a protocol refusal's kind, for a tally.
fn kind_of(refusal: &Refusal) -> &'static str {
  match refusal {
    Refusal::PktLine(_) => "PktLine",
    Refusal::Truncated { .. } => "Truncated",
    Refusal::UnexpectedPacket { .. } => "UnexpectedPacket",
    Refusal::Command { .. } => "Command",
    Refusal::Shallow { .. } => "Shallow",
    Refusal::Capabilities { .. } => "Capabilities",
    Refusal::ObjectFormat { .. } => "ObjectFormat",
    Refusal::PushCert { .. } => "PushCert",
    Refusal::Advertisement(_) => "Advertisement",
    Refusal::LineTooLong { .. } => "LineTooLong",
    Refusal::PastAdmission(_) => "PastAdmission",
    Refusal::AllocationRefused { .. } => "AllocationRefused",
  }
}

/// A-620 (hostile input: bit flips). Do: flip each bit of the atomic push's command list, one at a
/// time, receive it and write the answer. Expect: each refused by kind, or received and answered;
/// among the refusals a packet's length, a command, and the truncated request a lengthened packet
/// leaves.
#[test]
fn every_bit_flip_of_a_command_list_is_refused_by_kind_or_answered() {
  let atomic = scenario("atomic");
  let repository = Repo::from_pack(atomic.before_pack);
  let mut seen = Vec::new();
  for bit in 0..request_len(atomic.request) * 8 {
    let mut flipped = atomic.request.to_vec();
    flipped[bit / 8] ^= 1 << (bit % 8);
    let mut policy = git_policy(&repository);
    match receive(
      &flipped,
      &ADVERTISED,
      &repository,
      &HIDDEN,
      &mut policy,
      &mut unbounded(),
    ) {
      Ok(received) => {
        answer_to(&received);
        seen.push("answered");
      }
      Err(refusal) => seen.push(kind_of(&refusal)),
    }
  }
  for kind in ["answered", "PktLine", "Command", "Truncated"] {
    assert!(seen.contains(&kind), "no flip came to {kind}: {seen:?}");
  }
}

/// `data` as one zlib stream, deflated by this crate's engine: tests hold what streams mean, never
/// their bytes.
fn zlib(data: &[u8]) -> Vec<u8> {
  let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
  encoder.write_all(data).unwrap();
  encoder.finish().unwrap()
}

/// A pack's header: its signature, version 2, and `objects`.
fn pack_header(objects: u32) -> Vec<u8> {
  let mut bytes = pack::SIGNATURE.to_vec();
  bytes.extend_from_slice(&2u32.to_be_bytes());
  bytes.extend_from_slice(&objects.to_be_bytes());
  bytes
}

/// A pack of `objects`, each stored whole.
fn pack_of(objects: &[(Kind, &[u8])]) -> Vec<u8> {
  let mut bytes = pack_header(u32::try_from(objects.len()).unwrap());
  for (kind, content) in objects {
    let at = wide(bytes.len());
    let header = Header {
      representation: Representation::Whole(*kind),
      size: wide(content.len()),
    };
    entry::write(header, at, &mut bytes).unwrap();
    bytes.extend_from_slice(&zlib(content));
  }
  let checksum = pack::checksum_of(ObjectFormat::Sha1, &bytes).unwrap();
  bytes.extend_from_slice(checksum.as_bytes());
  bytes
}

/// A request as a client writes one: `shallow` lines, the commands (the first with
/// `capabilities`), a flush, the options and their flush when given, then `pack`.
fn request(
  shallow: &[ObjectId],
  commands: &[(ObjectId, ObjectId, &[u8])],
  capabilities: &[u8],
  options: &[&[u8]],
  pack: &[u8],
) -> Vec<u8> {
  let mut out = Vec::new();
  for id in shallow {
    let line = format!("shallow {id}");
    pktline::write(Packet::Data(line.as_bytes()), &mut out).unwrap();
  }
  for (index, (old, new, name)) in commands.iter().enumerate() {
    let mut line = format!("{old} {new} ").into_bytes();
    line.extend_from_slice(name);
    if index == 0 {
      line.push(0);
      line.extend_from_slice(capabilities);
    }
    pktline::write(Packet::Data(&line), &mut out).unwrap();
  }
  pktline::write(Packet::Flush, &mut out).unwrap();
  if !options.is_empty() {
    for option in options {
      pktline::write(Packet::Data(option), &mut out).unwrap();
    }
    pktline::write(Packet::Flush, &mut out).unwrap();
  }
  out.extend_from_slice(pack);
  out
}

/// The zero id.
fn zero() -> ObjectId {
  ObjectId::from_bytes(ObjectFormat::Sha1, &[0; 20]).unwrap()
}

/// A judge that accepts every command it is asked about, keeps their names, and appends each.
#[derive(Default)]
struct Applies(Vec<Vec<u8>>);

impl Judge for Applies {
  type Accepted = ();

  fn judge(&mut self, command: &Command, _pushed: &Pushed<'_>) -> Result<(), Declined> {
    self.0.push(command.name.clone());
    Ok(())
  }

  fn append(&mut self, _command: &Command, (): ()) -> Result<Vec<Reported>, Declined> {
    Ok(Vec::new())
  }

  fn append_atomic(
    &mut self,
    _accepted: Vec<(&Command, ())>,
  ) -> Result<Vec<Vec<Reported>>, Declined> {
    Ok(Vec::new())
  }
}

/// A commit naming `tree` and `parents`.
fn commit_text(tree: ObjectId, parents: &[ObjectId]) -> Vec<u8> {
  let mut text = format!("tree {tree}\n");
  for parent in parents {
    text.push_str(&format!("parent {parent}\n"));
  }
  text.push_str("author A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n\nm\n");
  text.into_bytes()
}

/// The outcomes of `bytes` received against `repository` with `hidden` hidden, every command
/// applied that the protocol's checks pass, and the names of those the judge was asked about.
fn outcomes_hiding(
  bytes: &[u8],
  repository: &Repo,
  hidden: &dyn HiddenRefs,
) -> (Vec<Outcome>, Vec<Vec<u8>>) {
  let mut judge = Applies::default();
  let received = receive(
    bytes,
    &ADVERTISED,
    repository,
    hidden,
    &mut judge,
    &mut unbounded(),
  );
  (received.unwrap().outcomes, judge.0)
}

/// The same, nothing hidden.
fn outcomes_of(bytes: &[u8], repository: &Repo) -> (Vec<Outcome>, Vec<Vec<u8>>) {
  outcomes_hiding(bytes, repository, &NoneHidden)
}

/// Whether `outcome` is a refusal for `reason`.
fn refused_for(outcome: &Outcome, reason: &[u8]) -> bool {
  matches!(outcome, Outcome::Ng(declined) if declined.reason == reason)
}

/// A-620 (connectivity: `set_connectivity_errors`). Do: push a commit whose tree is in neither the
/// pack nor the repository; one whose tree the repository holds; and a tag naming a commit nothing
/// holds. Expect: the first and the third refused `missing necessary objects` without being judged;
/// the second judged and applied.
#[test]
fn a_tip_that_reaches_an_object_nothing_holds_is_missing_necessary_objects() {
  let mut repository = Repo::default();
  let held_tree = repository.with(Kind::Tree, b"");
  let absent_tree = name_of(
    ObjectFormat::Sha1,
    Kind::Tree,
    b"100644 f\0aaaaaaaaaaaaaaaaaaaa",
  )
  .unwrap();
  let lacking = commit_text(absent_tree, &[]);
  let complete = commit_text(held_tree, &[]);
  let absent_commit = name_of(ObjectFormat::Sha1, Kind::Commit, b"never pushed").unwrap();
  let tag =
    format!("object {absent_commit}\ntype commit\ntag t\ntagger A <a@example.com> 1 +0000\n\nm\n")
      .into_bytes();
  let id = |kind, text: &[u8]| name_of(ObjectFormat::Sha1, kind, text).unwrap();
  let pack = pack_of(&[
    (Kind::Commit, &lacking),
    (Kind::Commit, &complete),
    (Kind::Tag, &tag),
  ]);
  let commands: [(ObjectId, ObjectId, &[u8]); 3] = [
    (zero(), id(Kind::Commit, &lacking), b"refs/heads/lacking"),
    (zero(), id(Kind::Commit, &complete), b"refs/heads/complete"),
    (zero(), id(Kind::Tag, &tag), b"refs/tags/t"),
  ];
  let bytes = request(&[], &commands, b" report-status", &[], &pack);
  let (outcomes, judged) = outcomes_of(&bytes, &repository);
  let missing = b"missing necessary objects";
  assert!(refused_for(&outcomes[0], missing), "{outcomes:?}");
  assert_eq!(outcomes[1], Outcome::Ok(Vec::new()));
  assert!(refused_for(&outcomes[2], missing), "{outcomes:?}");
  assert_eq!(judged, [b"refs/heads/complete".to_vec()]);
}

/// A-620 (shallow lines: `update_shallow_info`). Do: push a commit whose parent nothing holds, once
/// with the client calling it shallow and once without. Expect: `shallow update not allowed` with
/// the shallow line, `missing necessary objects` without it.
#[test]
fn a_shallow_commits_missing_parent_is_a_shallow_update_not_allowed() {
  let mut repository = Repo::default();
  let tree = repository.with(Kind::Tree, b"");
  let absent_parent = name_of(ObjectFormat::Sha1, Kind::Commit, b"an unknown parent").unwrap();
  let shallow = commit_text(tree, &[absent_parent]);
  let id = name_of(ObjectFormat::Sha1, Kind::Commit, &shallow).unwrap();
  let pack = pack_of(&[(Kind::Commit, &shallow)]);
  let command: [(ObjectId, ObjectId, &[u8]); 1] = [(zero(), id, b"refs/heads/shallow")];
  let with = request(&[id], &command, b" report-status", &[], &pack);
  let without = request(&[], &command, b" report-status", &[], &pack);
  assert!(refused_for(
    &outcomes_of(&with, &repository).0[0],
    b"shallow update not allowed"
  ));
  assert!(refused_for(
    &outcomes_of(&without, &repository).0[0],
    b"missing necessary objects"
  ));
}

/// A-620 (`atomic`: "Either all refs are updated or none", gitprotocol-capabilities). Do: push two
/// commands, one whose tip is missing objects, with `atomic` and without. Expect: with `atomic`,
/// the other refused `atomic push failure` and nothing judged (git 2.50.1 would still update it,
/// the check failing before its transaction); without, the other judged and applied.
#[test]
fn an_atomic_push_with_one_refusal_applies_no_ref() {
  let mut repository = Repo::default();
  let tree = repository.with(Kind::Tree, b"");
  let complete = commit_text(tree, &[]);
  let absent_tree = name_of(ObjectFormat::Sha1, Kind::Tree, b"absent").unwrap();
  let lacking = commit_text(absent_tree, &[]);
  let id = |text: &[u8]| name_of(ObjectFormat::Sha1, Kind::Commit, text).unwrap();
  let pack = pack_of(&[(Kind::Commit, &complete), (Kind::Commit, &lacking)]);
  let commands: [(ObjectId, ObjectId, &[u8]); 2] = [
    (zero(), id(&complete), b"refs/heads/good"),
    (zero(), id(&lacking), b"refs/heads/bad"),
  ];
  let atomic = request(&[], &commands, b" report-status atomic", &[], &pack);
  let (outcomes, judged) = outcomes_of(&atomic, &repository);
  assert!(
    refused_for(&outcomes[0], b"atomic push failure"),
    "{outcomes:?}"
  );
  assert!(
    refused_for(&outcomes[1], b"missing necessary objects"),
    "{outcomes:?}"
  );
  assert!(judged.is_empty());
  let plain = request(&[], &commands, b" report-status", &[], &pack);
  let (outcomes, judged) = outcomes_of(&plain, &repository);
  assert_eq!(outcomes[0], Outcome::Ok(Vec::new()));
  assert_eq!(judged, [b"refs/heads/good".to_vec()]);
}

/// The outcome of a push of one command on `name`, a delete of it or a create at a pushed blob,
/// and whether the judge was asked about it.
fn one_command_outcome(name: &[u8], delete: bool) -> (Outcome, bool) {
  let some = name_of(ObjectFormat::Sha1, Kind::Blob, b"x").unwrap();
  let (old, new, pack) = if delete {
    (some, zero(), Vec::new())
  } else {
    (zero(), some, pack_of(&[(Kind::Blob, b"x")]))
  };
  let capabilities = b" report-status side-band-64k";
  let bytes = request(&[], &[(old, new, name)], capabilities, &[], &pack);
  let (mut outcomes, judged) = outcomes_of(&bytes, &Repo::default());
  (outcomes.remove(0), !judged.is_empty())
}

/// A-620 (`update`: "funny refname"). Do: delete refs of names git refuses (`HEAD`, outside
/// `refs/`, `..`, a `.lock` component, a control byte) and of one level under `refs/`, and create
/// one of one level. Expect: each refused name `funny refname` with git's band-2 line, unjudged;
/// the one-level delete judged and applied, as git allows it.
#[test]
fn a_ref_name_git_refuses_is_a_funny_refname() {
  let cases: [(&[u8], bool, bool); 7] = [
    (b"HEAD", true, true),
    (b"heads/main", true, true),
    (b"refs/heads/a..b", true, true),
    (b"refs/heads/x.lock", true, true),
    (b"refs/heads/tab\there", true, true),
    (b"refs/one", true, false),
    (b"refs/one", false, true),
  ];
  for (name, delete, funny) in cases {
    let line = [b"refusing to update funny ref '", name, b"' remotely"].concat();
    let expected = if funny {
      Outcome::Ng(Declined {
        reason: b"funny refname".to_vec(),
        hook: Vec::new(),
        message: Some(message(&line)),
      })
    } else {
      Outcome::Ok(Vec::new())
    };
    let label = String::from_utf8_lossy(name);
    assert_eq!(
      one_command_outcome(name, delete),
      (expected, !funny),
      "{label}"
    );
  }
}

/// A-620 (the protocol's refusals, as git's `read_head_info` dies on them). Do: read requests with
/// another `object-format`, none named to a SHA-256 repository, a push certificate, capabilities
/// on a second command, a shallow line after a command, a command short of its name's space, an id
/// that is not hex, a delimiter packet, and a length that is not hex. Expect: each refused by its
/// own kind, at its packet.
#[test]
fn each_request_the_protocol_does_not_allow_is_refused_by_kind() {
  let id = name_of(ObjectFormat::Sha1, Kind::Blob, b"x").unwrap();
  let line = |text: &str| {
    let mut out = Vec::new();
    pktline::write(Packet::Data(text.as_bytes()), &mut out).unwrap();
    out
  };
  let command = format!("{} {id} refs/heads/x", zero());
  let first = line(&format!("{command}\0 report-status"));
  let second_at = wide(first.len());
  let not_hex = format!(
    "{} {}x refs/heads/x",
    zero(),
    id.to_string().get(1..).unwrap()
  );
  let cases: Vec<(Vec<u8>, Refusal)> = vec![
    (
      line(&format!("{command}\0 object-format=sha256")),
      Refusal::ObjectFormat { at: 0 },
    ),
    (
      line("push-cert\0 report-status"),
      Refusal::PushCert { at: 0 },
    ),
    (
      [first.clone(), line(&format!("{command}\0 atomic"))].concat(),
      Refusal::Capabilities { at: second_at },
    ),
    (
      [first.clone(), line(&format!("shallow {id}"))].concat(),
      Refusal::Shallow { at: second_at },
    ),
    (
      line(&format!("{} {id}", zero())),
      Refusal::Command { at: 0 },
    ),
    (line(&not_hex), Refusal::Command { at: 0 }),
    (b"0001".to_vec(), Refusal::UnexpectedPacket { at: 0 }),
    (
      b"00g0".to_vec(),
      Refusal::PktLine(pktline::Refusal::NotHex { at: 2 }),
    ),
  ];
  for (bytes, refusal) in cases {
    let mut reader = RequestReader::new(&ADVERTISED);
    assert_eq!(
      reader.read(&bytes, &mut unbounded()),
      Err(refusal),
      "{}",
      String::from_utf8_lossy(&bytes)
    );
  }
  let sha256 = Advertised {
    format: ObjectFormat::Sha256,
    ..ADVERTISED
  };
  let sha256_id = name_of(ObjectFormat::Sha256, Kind::Blob, b"x").unwrap();
  let zero256 = ObjectId::from_bytes(ObjectFormat::Sha256, &[0; 32]).unwrap();
  let bytes = line(&format!(
    "{zero256} {sha256_id} refs/heads/x\0 report-status"
  ));
  let mut reader = RequestReader::new(&sha256);
  assert_eq!(
    reader.read(&bytes, &mut unbounded()),
    Err(Refusal::ObjectFormat { at: 0 })
  );
}

/// `texts` as report lines, each its own packet ending with LF, then a flush.
fn report_lines(texts: &[String]) -> Vec<u8> {
  let mut out = Vec::new();
  for text in texts {
    pktline::write(Packet::Data(format!("{text}\n").as_bytes()), &mut out).unwrap();
  }
  pktline::write(Packet::Flush, &mut out).unwrap();
  out
}

/// A received push of three deletes: the first applied with two reports a hook made of it, the
/// second applied plainly, the third refused with a band-2 line; asked as report-status.
fn three_reported(id: ObjectId) -> Received {
  let commands: [(ObjectId, ObjectId, &[u8]); 3] = [
    (id, zero(), b"refs/for/main"),
    (id, zero(), b"refs/heads/b"),
    (id, zero(), b"refs/heads/c"),
  ];
  let bytes = request(&[], &commands, b" report-status", &[], &[]);
  let repository = Repo::default();
  let mut judge = Applies::default();
  let mut received = receive(
    &bytes,
    &ADVERTISED,
    &repository,
    &NoneHidden,
    &mut judge,
    &mut unbounded(),
  )
  .unwrap();
  let first = Reported {
    refname: Some(b"refs/pull/1/head".to_vec()),
    old: Some(zero()),
    new: Some(id),
    forced: true,
  };
  let second = Reported {
    refname: Some(b"refs/pull/2/head".to_vec()),
    ..Reported::default()
  };
  received.outcomes[0] = Outcome::Ok(vec![first, second]);
  received.outcomes[2] = Outcome::Ng(Declined {
    reason: b"hook declined".to_vec(),
    hook: Vec::new(),
    message: Some(message(b"hook declined to update refs/heads/c")),
  });
  received
}

/// A-620 (report-status and report-status-v2 as `report` and `report_v2` write them). Do: answer a
/// received push of three commands, one applied with two reports a hook made of it, one applied
/// plainly, one refused, as report-status, as report-status-v2, multiplexed and not, and with
/// neither asked for. Expect: v1 with no option lines; v2 with the first report's four option
/// lines, `ok` again, the second's refname line; band 1 and a last flush only when multiplexed;
/// nothing but the band-2 line and the flush when no report was asked for.
#[test]
fn reports_are_written_as_git_writes_them() {
  let id = name_of(ObjectFormat::Sha1, Kind::Blob, b"x").unwrap();
  let mut received = three_reported(id);
  let v1 = report_lines(&[
    "unpack ok".into(),
    "ok refs/for/main".into(),
    "ok refs/heads/b".into(),
    "ng refs/heads/c hook declined".into(),
  ]);
  let v2 = report_lines(&[
    "unpack ok".into(),
    "ok refs/for/main".into(),
    "option refname refs/pull/1/head".into(),
    format!("option old-oid {}", zero()),
    format!("option new-oid {id}"),
    "option forced-update".into(),
    "ok refs/for/main".into(),
    "option refname refs/pull/2/head".into(),
    "ok refs/heads/b".into(),
    "ng refs/heads/c hook declined".into(),
  ]);
  let mut band_two = Vec::new();
  let declined = b"error: hook declined to update refs/heads/c\n";
  sideband::write(Band::Progress, declined, &mut band_two);
  for (v2_asked, multiplexed, expected) in
    [(false, false, &v1), (true, false, &v2), (true, true, &v2)]
  {
    received.request.requested.report_status_v2 = v2_asked;
    received.request.requested.side_band_64k = multiplexed;
    let wanted = if multiplexed {
      let mut wanted = band_two.clone();
      sideband::write(Band::Data, expected, &mut wanted);
      wanted.extend_from_slice(b"0000");
      wanted
    } else {
      expected.clone()
    };
    let answer = answer_to(&received);
    assert_eq!(answer, wanted, "v2 {v2_asked}, multiplexed {multiplexed}");
  }
  received.request.requested.report_status = false;
  received.request.requested.report_status_v2 = false;
  assert_eq!(answer_to(&received), [band_two, b"0000".to_vec()].concat());
}

/// A pack of one reference delta whose base nothing holds.
fn thin_pack() -> Vec<u8> {
  let absent = name_of(ObjectFormat::Sha1, Kind::Blob, b"absent").unwrap();
  let mut thin = pack_header(1);
  let header = Header {
    representation: Representation::RefDelta { base: absent },
    size: 5,
  };
  entry::write(header, entry::PACK_HEADER_LEN, &mut thin).unwrap();
  thin.extend_from_slice(&zlib(&[6, 1, 0x91, 0, 1]));
  let checksum = pack::checksum_of(ObjectFormat::Sha1, &thin).unwrap();
  thin.extend_from_slice(checksum.as_bytes());
  thin
}

/// Asserts that a push of a create of `refs/heads/x` with `pack` is answered `words`, its command
/// `unpacker error`, under report-status.
fn assert_pack_answered(pack: &[u8], words: &[u8]) {
  let id = name_of(ObjectFormat::Sha1, Kind::Blob, b"x").unwrap();
  let command: [(ObjectId, ObjectId, &[u8]); 1] = [(zero(), id, b"refs/heads/x")];
  let bytes = request(&[], &command, b" report-status", &[], pack);
  let repository = Repo::default();
  let mut judge = Applies::default();
  let received = receive(
    &bytes,
    &ADVERTISED,
    &repository,
    &NoneHidden,
    &mut judge,
    &mut unbounded(),
  )
  .unwrap();
  let mut expected = Vec::new();
  pktline::write(Packet::Data(&[words, b"\n"].concat()), &mut expected).unwrap();
  pktline::write(
    Packet::Data(b"ng refs/heads/x unpacker error\n"),
    &mut expected,
  )
  .unwrap();
  pktline::write(Packet::Flush, &mut expected).unwrap();
  assert_eq!(
    answer_to(&received),
    expected,
    "{}",
    String::from_utf8_lossy(words)
  );
  assert!(judge.0.is_empty());
}

/// A-620 (the pack's status in git's words: `parse_pack_header`, `unpack`'s `unpack_limit`). Do:
/// push a pack of a wrong signature, of version 4, cut inside its header, one naming a base nothing
/// holds, and headers counting 99 and 100 objects with nothing after them. Expect: `unpack protocol
/// error (pack signature mismatch detected)`, `unpack protocol error (pack version unsupported)`,
/// `unpack eof before pack header was fully read`, `unpack unpack-objects abnormal exit` for the
/// one-object pack and the 99, `unpack index-pack abnormal exit` for the 100; every command
/// `unpacker error`, nothing judged.
#[test]
fn a_refused_pack_is_answered_in_gits_words() {
  let good = pack_of(&[(Kind::Blob, b"x")]);
  let mut signature = good.clone();
  signature[3] = b'X';
  let mut version = good.clone();
  version[7] = 4;
  let below = UNPACK_LIMIT - 1;
  let cases: [(Vec<u8>, &[u8]); 6] = [
    (
      signature,
      b"unpack protocol error (pack signature mismatch detected)",
    ),
    (version, b"unpack protocol error (pack version unsupported)"),
    (
      good[..7].to_vec(),
      b"unpack eof before pack header was fully read",
    ),
    (thin_pack(), b"unpack unpack-objects abnormal exit"),
    (pack_header(below), b"unpack unpack-objects abnormal exit"),
    (
      pack_header(UNPACK_LIMIT),
      b"unpack index-pack abnormal exit",
    ),
  ];
  for (pack, words) in cases {
    assert_pack_answered(&pack, words);
  }
}

/// Shape: the objects `refs/tags/v0.99` of git/git brings to an empty repository, the push stock git's client sent
/// when A-704 was found (2026-11-19, Mac Studio).
const V0_99_OBJECTS: u32 = 4_508;

/// Shape: an admission that holds a one-command request and its checks but not the records of
/// [`V0_99_OBJECTS`] objects.
const REQUEST_ROOM: u64 = 1 << 16;

/// A-704 (git's words for a pack past its room: `index-pack`'s `die(_("pack exceeds maximum
/// allowed size"))` under `receive.maxInputSize`, which receive-pack's `unpack` answers `index-pack
/// abnormal exit`). Do: push, multiplexed, a pack whose header counts [`V0_99_OBJECTS`] objects
/// under an admission that holds its request but not the records its header counts. Expect: band
/// 2 git's `fatal: pack exceeds maximum allowed size`; the report `unpack index-pack abnormal exit`,
/// never `eof before pack header was fully read`, and the command `unpacker error`; nothing
/// judged.
#[test]
fn a_pack_past_its_room_is_refused_in_the_words_of_gits_max_input_size() {
  let id = name_of(ObjectFormat::Sha1, Kind::Commit, b"x").unwrap();
  let command: [(ObjectId, ObjectId, &[u8]); 1] = [(zero(), id, b"refs/tags/v0.99")];
  let capabilities = b" report-status side-band-64k";
  let pack = pack_header(V0_99_OBJECTS);
  let bytes = request(&[], &command, capabilities, &[], &pack);
  let mut judge = Applies::default();
  let mut admission = Admission::new(REQUEST_ROOM);
  let received = receive(
    &bytes,
    &ADVERTISED,
    &Repo::default(),
    &NoneHidden,
    &mut judge,
    &mut admission,
  )
  .unwrap();
  let (report, text) = bands(&answer_to(&received));
  let mut expected = Vec::new();
  for line in [
    b"unpack index-pack abnormal exit\n".as_slice(),
    b"ng refs/tags/v0.99 unpacker error\n",
  ] {
    pktline::write(Packet::Data(line), &mut expected).unwrap();
  }
  pktline::write(Packet::Flush, &mut expected).unwrap();
  assert_eq!(
    (
      String::from_utf8_lossy(&report),
      String::from_utf8_lossy(&text)
    ),
    (
      String::from_utf8_lossy(&expected),
      "fatal: pack exceeds maximum allowed size\n".into()
    )
  );
  assert!(judge.0.is_empty());
}

/// `options`'s push received under `admission`.
fn options_received(options: &Scenario, admission: &mut Admission) -> Result<Received, Refusal> {
  let repository = Repo::from_pack(options.before_pack);
  let mut policy = git_policy(&repository);
  receive(
    options.request,
    &ADVERTISED,
    &repository,
    &HIDDEN,
    &mut policy,
    admission,
  )
}

/// A-620 (every size charged to the admission before it is built). Do: receive the options push
/// under exactly what receiving it charges, one byte less, and a request of a thousand commands
/// under a hundred kibibytes. Expect: the first received with the admission spent; the second
/// refused by an admission, the protocol's or the pack's; the third refused by the protocol's
/// admission while its commands are read.
#[test]
fn every_size_is_charged_to_the_admission_before_it_is_built() {
  let options = scenario("options");
  let mut measure = unbounded();
  options_received(&options, &mut measure).unwrap();
  let charged = u64::MAX - measure.left();
  let mut exact = Admission::new(charged);
  let received = options_received(&options, &mut exact).unwrap();
  assert_eq!(exact.left(), 0);
  assert_eq!(received.outcomes, [Outcome::Ok(Vec::new())]);
  let past = match options_received(&options, &mut Admission::new(charged - 1)) {
    Err(Refusal::PastAdmission(_)) => true,
    Ok(received) => matches!(received.unpacked, Some(Err(_))),
    Err(_) => false,
  };
  assert!(past);
  let id = name_of(ObjectFormat::Sha1, Kind::Blob, b"x").unwrap();
  let names: Vec<Vec<u8>> = (0..1000)
    .map(|n| format!("refs/heads/{n:0>200}").into_bytes())
    .collect();
  let commands: Vec<(ObjectId, ObjectId, &[u8])> = names
    .iter()
    .map(|name| (id, zero(), name.as_slice()))
    .collect();
  let bytes = request(&[], &commands, b" report-status", &[], &[]);
  let mut reader = RequestReader::new(&ADVERTISED);
  let refused = reader.read(&bytes, &mut Admission::new(100 << 10));
  assert!(
    matches!(refused, Err(Refusal::PastAdmission(PastAdmission { .. }))),
    "{refused:?}"
  );
}

/// A-620 (connectivity alone). Do: check the connectivity of the update push's commands against
/// its server, and against an empty repository. Expect: complete against its server; missing
/// against nothing, the thin pack's base and the old history being nowhere.
#[test]
fn connectivity_is_complete_against_the_server_and_missing_against_nothing() {
  let update = scenario("update");
  let repository = Repo::from_pack(update.before_pack);
  let received = received(&update, &repository);
  let unpacked = received.unpacked.as_ref().unwrap().as_ref().unwrap();
  let pushed = Pushed::new(&unpacked.objects, &mut unbounded()).unwrap();
  let against_server =
    connectivity(&received.request, &pushed, &repository, &mut unbounded()).unwrap();
  assert_eq!(against_server, [Connected::Complete]);
  let nothing = connectivity(
    &received.request,
    &pushed,
    &Repo::default(),
    &mut unbounded(),
  )
  .unwrap();
  assert_eq!(nothing, [Connected::Missing]);
}
