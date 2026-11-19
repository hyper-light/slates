//! Tests of `getbundle` (A-619 part 2). The golden: the three sessions stock `hg serve --stdio` 7.2 answered
//! (`fixtures/serve/make-serve-fixtures.sh`: a clone; a pull after a public change under a bookmark; an incoming after
//! a draft one), each `getbundle` asked again of a store rebuilt from what 7.2 had sent up to it, and the answers
//! compared by meaning: the same parts with the same parameters, every payload but the changegroup byte for byte, and
//! the changegroup as each revision's node, parents, link, flags and full text in order, every node the hash of its
//! parents and text. The model: on generated histories with files, what this module sends and what 7.2's packer,
//! stated literally, sends (each manifest's delta read against the parent it is stored against, or whole) agree on
//! every manifest and on every file revision the client lacks. Hostile requests, and a store that lost a text, are
//! refused by kind.

use super::super::discovery::head_revs;
use super::super::pull::{self, Outgoing};
use super::super::session::{self, Refused, Step};
use super::super::tests::{
  Changeset, History, ancestors, at, captured, hex, history, joined, node_for, room,
};
use super::super::{Refusal, Repository, Rev};
use super::{Asked, Logged, Store, changegroup, getbundle};
use crate::bundle2::{Bundle2, Param, Part};
use crate::capabilities;
use crate::changegroup::{Changegroup, Revision, Version};
use crate::manifest::{self, Flag};
use crate::obsmarkers::{self, Marker};
use crate::phases::Phase;
use crate::ssh::{self, Read};
use crate::wire::{Arguments, Spec};
use crate::{Admission, NULL_NODE, Node, changeset, delta, node};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A revision of a manifest or file log the test's store keeps.
#[derive(Clone, Debug)]
struct Kept {
  position: u64,
  parents: [Node; 2],
  link: Node,
  flags: u16,
  text: Vec<u8>,
}

impl Kept {
  fn logged(&self) -> Logged<'_> {
    Logged {
      position: self.position,
      parents: self.parents,
      link: self.link,
      flags: self.flags,
      text: &self.text,
    }
  }
}

/// A manifest or file log: each revision by node.
type Log = HashMap<Node, Kept>;

/// The test's [`Store`]: a served view with its changeset texts, its manifest and file logs, and its markers, each
/// marker alone in a payload of its own.
#[derive(Clone, Debug, Default)]
struct Served {
  history: History,
  changesets: HashMap<Node, Vec<u8>>,
  manifests: Log,
  files: HashMap<Vec<u8>, Log>,
  markers: Vec<Vec<u8>>,
}

impl Repository for Served {
  fn count(&self) -> Rev {
    self.history.count()
  }
  fn node(&self, rev: Rev) -> Option<Node> {
    self.history.node(rev)
  }
  fn rev(&self, node: &Node) -> Option<Rev> {
    self.history.rev(node)
  }
  fn parents(&self, rev: Rev) -> [Option<Rev>; 2] {
    self.history.parents(rev)
  }
  fn branch(&self, rev: Rev) -> &[u8] {
    self.history.branch(rev)
  }
  fn closes(&self, rev: Rev) -> bool {
    self.history.closes(rev)
  }
  fn phase(&self, rev: Rev) -> Phase {
    self.history.phase(rev)
  }
  fn publishing(&self) -> bool {
    self.history.publishing()
  }
  fn bookmarks(&self) -> impl Iterator<Item = (&[u8], Node)> {
    self.history.bookmarks()
  }
}

impl Store for Served {
  fn changeset_text(&self, rev: Rev) -> Option<&[u8]> {
    self
      .changesets
      .get(&self.history.node(rev)?)
      .map(Vec::as_slice)
  }
  fn manifest(&self, node: &Node) -> Option<Logged<'_>> {
    self.manifests.get(node).map(Kept::logged)
  }
  fn file(&self, path: &[u8], node: &Node) -> Option<Logged<'_>> {
    self.files.get(path)?.get(node).map(Kept::logged)
  }
  fn file_node(&self, manifest_node: &Node, path: &[u8]) -> Option<Node> {
    self.entries(manifest_node).remove(path)
  }
  fn markers(&self) -> impl Iterator<Item = Marker<'_>> {
    self.markers.iter().flat_map(|payload| {
      obsmarkers::decode(payload)
        .ok()
        .into_iter()
        .flat_map(|markers| markers.iter())
    })
  }
}

/// Which log a revision belongs to.
#[derive(Clone, Copy)]
enum Named<'a> {
  Changelog,
  Manifests,
  File(&'a [u8]),
}

impl Served {
  /// A manifest's entries by path; none for the null manifest or one the store lacks.
  fn entries(&self, manifest_node: &Node) -> BTreeMap<Vec<u8>, Node> {
    self
      .manifests
      .get(manifest_node)
      .map_or_else(BTreeMap::new, |kept| {
        manifest::decode(&kept.text)
          .unwrap()
          .map(|entry| (entry.path.to_vec(), entry.node))
          .collect()
      })
  }

  /// A revision's full text as the store keeps it.
  fn text(&self, named: Named<'_>, node: &Node) -> Vec<u8> {
    match named {
      Named::Changelog => self.changesets[node].clone(),
      Named::Manifests => self.manifests[node].text.clone(),
      Named::File(path) => self.files[path][node].text.clone(),
    }
  }

  /// Takes in one answer 7.2 sent: each changegroup revision kept with its full text, and each marker kept once.
  fn take(&mut self, stream: &[u8]) {
    let bundle = Bundle2::decode(stream, &mut room()).unwrap();
    for (index, part) in bundle.top_level() {
      let payload = bundle.payload_bytes(index, &mut room()).unwrap();
      if part.is(b"changegroup") {
        let version = Version::from_part_param(part.param(b"version")).unwrap();
        self.take_changegroup(&Changegroup::decode(&payload, version, &mut room()).unwrap());
      } else if part.is(b"obsmarkers") {
        self.take_markers(&payload);
      }
    }
  }

  /// Keeps a changegroup's revisions, each text its delta applied to its base's (a revision kept before, or the
  /// null base's empty text), each log's positions in the order the revisions came.
  fn take_changegroup(&mut self, changegroup: &Changegroup<'_>) {
    for revision in changegroup.group(&changegroup.changelog) {
      let text = rebuilt(
        revision,
        self.changesets.get(&revision.base).map(Vec::as_slice),
      );
      self.changesets.insert(revision.node, text);
    }
    for revision in changegroup.group(&changegroup.manifests) {
      let base = self.manifests.get(&revision.base);
      let text = rebuilt(revision, base.map(|kept| kept.text.as_slice()));
      keep(&mut self.manifests, revision, text);
    }
    for group in &changegroup.files {
      let log = self.files.entry(group.name.to_vec()).or_default();
      for revision in changegroup.group(&group.revisions) {
        let text = rebuilt(
          revision,
          log.get(&revision.base).map(|kept| kept.text.as_slice()),
        );
        keep(log, revision, text);
      }
    }
  }

  /// Keeps each marker of a payload alone in its payload's format, once.
  fn take_markers(&mut self, payload: &[u8]) {
    let markers = obsmarkers::decode(payload).unwrap();
    for marker in markers.iter() {
      let mut alone = Vec::new();
      obsmarkers::encode(markers.format(), &[marker], &mut alone).unwrap();
      if !self.markers.contains(&alone) {
        self.markers.push(alone);
      }
    }
  }
}

/// A revision's full text: its delta applied to its base's text, the null base's empty.
fn rebuilt(revision: &Revision<'_>, base: Option<&[u8]>) -> Vec<u8> {
  let base = if revision.base == NULL_NODE {
    &[][..]
  } else {
    base.unwrap()
  };
  delta::apply(base, revision.data, &mut room()).unwrap()
}

/// Keeps a revision in its log at the next position, once.
fn keep(log: &mut Log, revision: &Revision<'_>, text: Vec<u8>) {
  let position = u64::try_from(log.len()).unwrap();
  log.entry(revision.node).or_insert(Kept {
    position,
    parents: [revision.p1, revision.p2],
    link: revision.link,
    flags: revision.flags,
    text,
  });
}

/// One session the script captured: what the client wrote, what the server answered, and the served view and its
/// bookmarks as the script dumped them before it.
#[derive(Clone, Copy)]
struct Session {
  client: &'static [u8],
  server: &'static [u8],
  state: &'static str,
  bookmarks: &'static str,
}

/// The sessions, in the order the script ran them.
const SESSIONS: [Session; 3] = [
  Session {
    client: include_bytes!("../../fixtures/serve/session-clone.client"),
    server: include_bytes!("../../fixtures/serve/session-clone.server"),
    state: include_str!("../../fixtures/serve/state-clone.txt"),
    bookmarks: include_str!("../../fixtures/serve/bookmarks-clone.txt"),
  },
  Session {
    client: include_bytes!("../../fixtures/serve/session-pull.client"),
    server: include_bytes!("../../fixtures/serve/session-pull.server"),
    state: include_str!("../../fixtures/serve/state-pull.txt"),
    bookmarks: include_str!("../../fixtures/serve/bookmarks-pull.txt"),
  },
  Session {
    client: include_bytes!("../../fixtures/serve/session-incoming.client"),
    server: include_bytes!("../../fixtures/serve/session-incoming.server"),
    state: include_str!("../../fixtures/serve/state-incoming.txt"),
    bookmarks: include_str!("../../fixtures/serve/bookmarks-incoming.txt"),
  },
];

/// 7.2's argument spec for each command the sessions send (`wireprotov1server`).
fn spec(command: &[u8]) -> Spec<'static> {
  Spec(match command {
    b"between" => b"pairs",
    b"protocaps" => b"caps",
    b"batch" => b"cmds *",
    b"known" => b"nodes *",
    b"getbundle" => b"*",
    b"listkeys" => b"namespace",
    _ => b"",
  })
}

/// A request: its command's name and its arguments.
type Request<'a> = (&'a [u8], Arguments<'a>);

/// A session's requests, read with the SSH version 1 codec.
fn requests(mut client: &[u8]) -> Vec<Request<'_>> {
  let longest = b"getbundle".len();
  let mut requests = Vec::new();
  while !client.is_empty() {
    let Ok(Read::Done(command, used)) = ssh::command(client, longest) else {
      panic!("a whole command line");
    };
    client = &client[used..];
    let Ok(Read::Done(arguments, used)) = ssh::arguments(client, spec(command), &mut room()) else {
      panic!("whole arguments");
    };
    client = &client[used..];
    requests.push((command, arguments));
  }
  requests
}

/// The server's answers, one for each request: a string, `<length>\n<value>`, but for `getbundle` the raw bundle2
/// stream, which ends itself.
fn answers<'s>(mut server: &'s [u8], requests: &[Request<'_>]) -> Vec<&'s [u8]> {
  let mut answers = Vec::new();
  for (command, _) in requests {
    let (answer, used) = if *command == b"getbundle" {
      let len = stream_len(server);
      (&server[..len], len)
    } else {
      let Ok(Read::Done(value, used)) = ssh::string(server, &room()) else {
        panic!("a whole answer");
      };
      (value, used)
    };
    answers.push(answer);
    server = &server[used..];
  }
  assert!(server.is_empty(), "every answer read");
  answers
}

/// The length of the bundle2 stream at the front of `bytes`: its magic and parameters, then each part's header and
/// payload chunks, up to the empty header that ends it (7.2's answers interrupt no part).
fn stream_len(bytes: &[u8]) -> usize {
  let word = |at: usize| {
    let field: [u8; 4] = bytes[at..at + 4].try_into().unwrap();
    usize::try_from(i32::from_be_bytes(field)).unwrap()
  };
  let mut at = 8 + word(4);
  loop {
    let header = word(at);
    at += 4;
    if header == 0 {
      return at;
    }
    at += header;
    loop {
      let chunk = word(at);
      at += 4 + chunk;
      if chunk == 0 {
        break;
      }
    }
  }
}

/// A `getbundle`'s arguments as [`Asked`].
fn asked<'a>(arguments: &Arguments<'a>) -> Asked<'a> {
  Asked {
    heads: arguments.entry(b"heads").unwrap_or_default(),
    common: arguments.entry(b"common").unwrap_or_default(),
    bundlecaps: arguments.entry(b"bundlecaps").unwrap_or_default(),
    cg: arguments.entry(b"cg"),
    phases: arguments.entry(b"phases"),
    bookmarks: arguments.entry(b"bookmarks"),
    obsmarkers: arguments.entry(b"obsmarkers"),
    listkeys: arguments.entry(b"listkeys"),
  }
}

/// One revision by meaning: its node, parents, link, flags and full text.
type Meant = (Node, [Node; 2], Node, u16, Vec<u8>);

/// A changegroup by meaning: the changelog's revisions, the manifests', and each file group's path and revisions.
#[derive(Debug, PartialEq, Eq)]
struct Meaning {
  changelog: Vec<Meant>,
  manifests: Vec<Meant>,
  files: Vec<(Vec<u8>, Vec<Meant>)>,
}

/// A changegroup's meaning, each revision's full text from `text`.
fn meaning(
  changegroup: &Changegroup<'_>,
  text: impl Fn(Named<'_>, &Revision<'_>) -> Vec<u8>,
) -> Meaning {
  let meant = |named: Named<'_>, revisions: &[Revision<'_>]| -> Vec<Meant> {
    revisions
      .iter()
      .map(|revision| {
        let parents = [revision.p1, revision.p2];
        (
          revision.node,
          parents,
          revision.link,
          revision.flags,
          text(named, revision),
        )
      })
      .collect()
  };
  Meaning {
    changelog: meant(Named::Changelog, changegroup.group(&changegroup.changelog)),
    manifests: meant(Named::Manifests, changegroup.group(&changegroup.manifests)),
    files: changegroup
      .files
      .iter()
      .map(|group| {
        let revisions = changegroup.group(&group.revisions);
        (
          group.name.to_vec(),
          meant(Named::File(group.name), revisions),
        )
      })
      .collect(),
  }
}

/// A full text this module sent: one hunk against the null base.
fn full_text(revision: &Revision<'_>) -> Vec<u8> {
  assert_eq!(
    revision.base, NULL_NODE,
    "every revision against the null base"
  );
  delta::apply(&[], revision.data, &mut room()).unwrap()
}

/// Whether every revision's node is the hash of its parents and full text.
fn hashes_hold(meaning: &Meaning) -> bool {
  let files = meaning.files.iter().flat_map(|(_, revisions)| revisions);
  meaning
    .changelog
    .iter()
    .chain(&meaning.manifests)
    .chain(files)
    .all(|(revision, [p1, p2], _, _, text)| {
      node::compute(p1, p2, text).is_ok_and(|hash| hash == *revision)
    })
}

/// A part's header: its id, its type as sent, and its mandatory and advisory parameters.
type Header = (
  u32,
  Vec<u8>,
  Vec<(Vec<u8>, Vec<u8>)>,
  Vec<(Vec<u8>, Vec<u8>)>,
);

/// Every top-level part's header.
fn headers(bundle: &Bundle2<'_>) -> Vec<Header> {
  let owned = |params: &[Param<'_>]| -> Vec<(Vec<u8>, Vec<u8>)> {
    params
      .iter()
      .map(|param| (param.key.to_vec(), param.value.to_vec()))
      .collect()
  };
  bundle
    .top_level()
    .map(|(_, part)| {
      (
        part.id,
        part.kind.to_vec(),
        owned(part.mandatory()),
        owned(part.advisory()),
      )
    })
    .collect()
}

/// Checks one `getbundle` against 7.2's answer: the stream's parameters, every part's header, each payload byte for
/// byte but the changegroup's, which is compared by meaning.
fn check_getbundle(served: &Served, arguments: &Arguments<'_>, theirs: &[u8]) {
  let mut ours = Vec::new();
  getbundle(served, &asked(arguments), &mut room(), &mut ours).unwrap();
  check_streams(served, &ours, theirs);
}

/// Checks a bundle2 stream against 7.2's: its parameters, every part's header, each payload byte for byte but the
/// changegroup's, which is compared by meaning.
fn check_streams(served: &Served, ours: &[u8], theirs: &[u8]) {
  let ours = Bundle2::decode(ours, &mut room()).unwrap();
  let theirs = Bundle2::decode(theirs, &mut room()).unwrap();
  assert_eq!(
    (&ours.params, headers(&ours)),
    (&theirs.params, headers(&theirs))
  );
  for ((index, part), (their_index, _)) in ours.top_level().zip(theirs.top_level()) {
    let our_payload = ours.payload_bytes(index, &mut room()).unwrap();
    let their_payload = theirs.payload_bytes(their_index, &mut room()).unwrap();
    if part.is(b"changegroup") {
      check_changegroup(served, part, &our_payload, &their_payload);
    } else {
      assert_eq!(
        our_payload,
        their_payload,
        "{}",
        String::from_utf8_lossy(part.kind)
      );
    }
  }
}

/// Checks a changegroup against 7.2's by meaning, every node of ours the hash of its parents and text.
fn check_changegroup(served: &Served, part: &Part<'_>, ours: &[u8], theirs: &[u8]) {
  let version = Version::from_part_param(part.param(b"version")).unwrap();
  let ours = Changegroup::decode(ours, version, &mut room()).unwrap();
  let theirs = Changegroup::decode(theirs, version, &mut room()).unwrap();
  let ours = meaning(&ours, |_, revision| full_text(revision));
  let theirs = meaning(&theirs, |named, revision| {
    served.text(named, &revision.node)
  });
  assert!(
    hashes_hold(&ours),
    "every node the hash of its parents and text"
  );
  assert_eq!(ours, theirs);
}

/// Checks a `listkeys` against 7.2's answer, byte for byte.
fn check_listkeys(served: &Served, arguments: &Arguments<'_>, theirs: &[u8]) {
  let namespace = arguments.get(spec(b"listkeys"), b"namespace").unwrap();
  let mut ours = Vec::new();
  pull::listkeys(served, namespace, &mut room(), &mut ours).unwrap();
  assert_eq!(
    String::from_utf8_lossy(&ours),
    String::from_utf8_lossy(theirs)
  );
}

/// AC-HG.1 (golden), A-619: do replay the clone, pull and incoming sessions of stock `hg serve --stdio` 7.2, each
/// `getbundle` asked again of a store holding what 7.2 had sent up to it, and the incoming's closing `listkeys` of the
/// phases. Expect each `getbundle` answered as 7.2 meant it: the clone's ten changesets, nine manifests (a closed
/// branch's head reuses its parent's) and their files, then the bookmarks, the bookmarks' keys, the amend's marker and
/// the phase heads; the pull's one public changeset with the same parts; the incoming's one draft changeset alone; and
/// the phases' keys byte for byte.
#[test]
fn every_session_getbundle_answers_as_mercurial_meant_it() {
  let mut served = Served::default();
  let mut checked = Vec::new();
  for session in SESSIONS {
    served.history = captured(session.state, session.bookmarks);
    let requests = requests(session.client);
    for ((command, arguments), answer) in requests.iter().zip(answers(session.server, &requests)) {
      if *command == b"getbundle" {
        served.take(answer);
        check_getbundle(&served, arguments, answer);
      } else if *command == b"listkeys" {
        check_listkeys(&served, arguments, answer);
      }
      checked.push(String::from_utf8_lossy(command).into_owned());
    }
  }
  let session = ["hello", "between", "protocaps", "batch", "getbundle"];
  assert_eq!(
    checked,
    [&session[..], &session, &session, &["listkeys"]].concat()
  );
}

/// The answers the SSH version 1 session gives a client's bytes, one per request, then the empty line that ends it.
fn serve_whole(served: &Served, client: &[u8]) -> Vec<Vec<u8>> {
  let input = [client, b"\n"].concat();
  let mut rest = input.as_slice();
  let mut answers = Vec::new();
  loop {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    match session::serve(served, rest, &mut room(), &mut stdout, &mut stderr) {
      Read::Done(Step::Answered { refused: None }, used) => {
        answers.push(stdout);
        rest = &rest[used..];
      }
      Read::Done(Step::Ended { refused: None }, used) if used == rest.len() => return answers,
      other => panic!("{other:?}: {}", String::from_utf8_lossy(&stderr)),
    }
  }
}

/// Checks `hello`'s answer: a string holding a capabilities line of what is served, each capability one 7.2
/// advertised with the same value, `bundle2`'s a subset of 7.2's.
fn check_hello(ours: &[u8], theirs: &[u8]) {
  let Ok(Read::Done(value, _)) = ssh::string(ours, &room()) else {
    panic!("a string answer");
  };
  let ours = capabilities::decode_hello(value).unwrap();
  let theirs = capabilities::decode_hello(theirs).unwrap();
  let names: Vec<&[u8]> = ours.iter().map(|(name, _)| name).collect();
  let served = [
    &b"batch"[..],
    b"branchmap",
    b"bundle2",
    b"getbundle",
    b"known",
    b"protocaps",
  ];
  assert_eq!(names, served);
  for (name, value) in ours.iter() {
    let held = theirs.get(name).unwrap();
    if name == capabilities::BUNDLE2 {
      check_bundle2_subset(value.unwrap(), held.unwrap());
    } else {
      assert_eq!(value, held);
    }
  }
}

/// Checks that every bundle2 capability of `ours`, and each of its values, is one of `theirs`.
fn check_bundle2_subset(ours: &[u8], theirs: &[u8]) {
  let (mut ours_quoted, mut theirs_quoted) = (Vec::new(), Vec::new());
  let (mut ours_unquoted, mut theirs_unquoted) = (Vec::new(), Vec::new());
  let ours = capabilities::decode_bundle2_value(ours, &mut room(), &mut ours_quoted).unwrap();
  let theirs = capabilities::decode_bundle2_value(theirs, &mut room(), &mut theirs_quoted).unwrap();
  let ours = capabilities::decode_bundle2(ours, &mut room(), &mut ours_unquoted).unwrap();
  let theirs = capabilities::decode_bundle2(theirs, &mut room(), &mut theirs_unquoted).unwrap();
  for (name, values) in ours.iter() {
    let held = theirs.get(name).unwrap();
    assert!(
      values.iter().all(|value| held.contains(value)),
      "{}",
      String::from_utf8_lossy(name)
    );
  }
}

/// Checks one answer of the session against 7.2's: `hello` by its capabilities, `getbundle` by meaning, every other
/// string byte for byte with its framing.
fn check_answer(served: &Served, command: &[u8], theirs: &[u8], ours: &[u8]) {
  match command {
    b"hello" => check_hello(ours, theirs),
    b"getbundle" => check_streams(served, ours, theirs),
    _ => {
      let mut framed = Vec::new();
      ssh::respond(theirs, &mut framed);
      assert_eq!(
        String::from_utf8_lossy(ours),
        String::from_utf8_lossy(&framed)
      );
    }
  }
}

/// AC-HG.1 (golden), A-619 part 3: do serve each captured session whole through the SSH version 1 session (the
/// client's bytes as 7.2's client wrote them, then the empty line that ends a session) from a store holding what 7.2
/// had sent up to it. Expect `between`, `protocaps`, `batch` and `listkeys` answered byte for byte as 7.2 answered,
/// `hello` a capabilities line of what is served (each capability one 7.2 advertised), every `getbundle` stream as 7.2
/// meant it, and the session ended by its empty line with all of its input taken.
#[test]
fn every_session_is_served_whole_as_mercurial_served_it() {
  let mut served = Served::default();
  let mut answered = 0;
  for session in SESSIONS {
    served.history = captured(session.state, session.bookmarks);
    let requests = requests(session.client);
    let theirs = answers(session.server, &requests);
    let at = requests
      .iter()
      .position(|(command, _)| *command == b"getbundle")
      .unwrap();
    served.take(theirs[at]);
    let ours = serve_whole(&served, session.client);
    assert_eq!(ours.len(), requests.len());
    for (((command, _), theirs), ours) in requests.iter().zip(&theirs).zip(&ours) {
      check_answer(&served, command, theirs, ours);
      answered += 1;
    }
  }
  assert_eq!(answered, 16);
}

/// What one session step answers `input` with: the step, stdout and stderr.
fn step(served: &Served, input: &[u8], room: u64) -> (Read<Step>, Vec<u8>, Vec<u8>) {
  let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
  let read = session::serve(
    served,
    input,
    &mut Admission::new(room),
    &mut stdout,
    &mut stderr,
  );
  (read, stdout, stderr)
}

/// A request of one command and its arguments as 7.2's client frames them, the dictionary's entries in order.
fn request(
  command: &[u8],
  named: &[(&[u8], &[u8])],
  dictionary: Option<&[(&[u8], &[u8])]>,
) -> Vec<u8> {
  let mut out = [command, b"\n"].concat();
  let line = |key: &[u8], len: usize| [key, b" ", len.to_string().as_bytes(), b"\n"].concat();
  if let Some(entries) = dictionary {
    out.extend_from_slice(&line(b"*", entries.len()));
    for (key, value) in entries {
      out.extend_from_slice(&[&line(key, value.len())[..], value].concat());
    }
  }
  for (key, value) in named {
    out.extend_from_slice(&[&line(key, value.len())[..], value].concat());
  }
  out
}

/// AC-HG.9 (hostile input), A-619 part 3: do send the session a name 7.2 lacks, a command 7.2 serves and this server
/// does not (with its argument), a batch calling `getbundle`, a batch call without its space, `known` of a node that
/// is not hex, and `getbundle` with no bundle2 capability. Expect an empty string for the unknown name; every other one
/// answered out of band (an empty line on stdout, the words and `\n-\n` on stderr) naming its refusal by kind, each
/// request taken whole, and the session going on.
#[test]
fn a_request_this_server_cannot_answer_is_refused_out_of_band() {
  let served = cloned();
  let unknown = step(&served, b"frobnicate\n", 1 << 20);
  assert_eq!(
    (unknown.0, unknown.1, unknown.2),
    (
      Read::Done(Step::Answered { refused: None }, 11),
      b"0\n".to_vec(),
      Vec::new()
    )
  );
  let batch_of = |cmds: &[u8]| request(b"batch", &[(b"cmds", cmds)], Some(&[]));
  for (input, refused) in [
    (
      request(b"lookup", &[(b"key", b"tip")], None),
      Refused::NotServed,
    ),
    (batch_of(b"heads ;getbundle "), Refused::NotServed),
    (
      batch_of(b"heads"),
      Refused::Batch(crate::batch::Refusal::Call { call: 0 }),
    ),
    (
      request(b"known", &[(b"nodes", b"xyz")], Some(&[])),
      Refused::Serve(Refusal::NodeList { at: 0 }),
    ),
    (
      request(b"getbundle", &[], Some(&[(b"common", b"")])),
      Refused::Serve(Refusal::NoBundle2),
    ),
  ] {
    let (read, stdout, stderr) = step(&served, &input, 1 << 20);
    assert_eq!(
      (read, stdout),
      (
        Read::Done(
          Step::Answered {
            refused: Some(refused)
          },
          input.len()
        ),
        b"\n".to_vec()
      )
    );
    assert!(stderr.ends_with(b"\n-\n") && stderr.len() > 3);
  }
}

/// AC-HG.9, A-619 part 3: do ask `getbundle` of a bundle2 client that lists only changegroup version 01. Expect, as 7.2
/// answers an abort of a bundle2 request, a bundle2 stream of one `ERROR:ABORT` part whose mandatory `message` is 7.2's
/// "no common changegroup version", and nothing on stderr.
#[test]
fn a_bundle2_request_refused_is_answered_with_an_abort_part() {
  let served = cloned();
  let old = caps(b"HG20\nchangegroup=01");
  let input = request(b"getbundle", &[], Some(&[(b"bundlecaps", &old)]));
  let (read, stdout, stderr) = step(&served, &input, 1 << 20);
  let refused = Refused::Serve(Refusal::NoCommonExchange {
    what: "changegroup version",
  });
  assert_eq!(
    (read, stderr),
    (
      Read::Done(
        Step::Answered {
          refused: Some(refused)
        },
        input.len()
      ),
      Vec::new()
    )
  );
  let stream = Bundle2::decode(&stdout, &mut room()).unwrap();
  let parts: Vec<(&[u8], Option<&[u8]>, usize)> = stream
    .top_level()
    .map(|(_, part)| (part.kind, part.param(b"message"), part.mandatory_params))
    .collect();
  assert_eq!(
    parts,
    [(
      &b"ERROR:ABORT"[..],
      Some(&b"no common changegroup version"[..]),
      1
    )]
  );
}

/// AC-HG.9, A-619 part 3: do send the empty line that ends a session, part of a command line, part of an argument, an
/// argument longer than the admission leaves, and a line longer than any command's. Expect the session ended by the
/// empty line; more asked for each part, never an answer; and the last two answered out of band and the session ended,
/// the rest of the input taken.
#[test]
fn a_session_ends_on_an_empty_line_or_an_unreadable_request() {
  let served = cloned();
  assert_eq!(
    step(&served, b"\n", 1 << 20).0,
    Read::Done(Step::Ended { refused: None }, 1)
  );
  for part in [&b"heads"[..], b"between\npairs 81\n000"] {
    let (read, stdout, _) = step(&served, part, 1 << 20);
    assert!(matches!(read, Read::More { needed } if needed > part.len()) && stdout.is_empty());
  }
  let long = [&b"between\npairs 99999\n"[..], &[b'0'; 64]].concat();
  let line = vec![b'x'; 64];
  for input in [long, line] {
    let (read, stdout, stderr) = step(&served, &input, 1 << 10);
    let ended = matches!(read, Read::Done(Step::Ended { refused: Some(Refused::Request(_)) }, used) if used == input.len());
    assert!(
      ended && stdout == b"\n" && stderr.ends_with(b"\n-\n"),
      "{read:?}"
    );
  }
}

/// Shape: the paths a generated history's changesets edit, few enough that branches often edit one alike.
const PATHS: [&[u8]; 3] = [b"a", b"b/c", b"d"];

/// What a generated revision does: the paths it edits, each an index into [`PATHS`] and the content byte it writes,
/// and how its manifest's delta is stored (0 against its first parent, 1 against its second where it has one, 2
/// whole), which 7.2's read of it follows.
type Edit = (Vec<(u8, u8)>, u8);

/// Revisions by their node and link, in order.
type Linked = Vec<(Node, Node)>;

/// The manifest and file revisions a changegroup carries, by group.
#[derive(Debug, PartialEq, Eq)]
struct Picked {
  manifests: Linked,
  files: Vec<(Vec<u8>, Linked)>,
}

/// A filled history's store, and each manifest's storage beside it.
type Filled = (Served, HashMap<Node, u8>);

/// Each manifest the missing changesets name with its first namer, and the paths they list.
type Namers = (Linked, BTreeSet<Vec<u8>>);

/// Each file revision a packing yields, by path, with its link.
type Yielded = BTreeMap<Vec<u8>, HashMap<Node, Node>>;

/// A generated history filled with files. A changeset's manifest is its first parent's; a merge's also holds what its
/// second parent holds, that parent's revision winning where both hold one (unlisted, as a merge that changed one side
/// only); each path it edits gets the revision whose text is the path and the content byte, over the one it held. A
/// changeset that edits nothing and merges nothing reuses its parent's manifest, as 7.2 reuses it, and a revision two
/// changesets make alike is kept once, linked to the first. Each manifest's storage is recorded beside the store.
fn filled(history: History, edits: &[Edit]) -> Filled {
  let mut served = Served {
    history,
    ..Served::default()
  };
  let mut stored = HashMap::new();
  let mut manifest_of = Vec::new();
  for rev in 0..served.history.count() {
    let (edited, storage) = edits.get(at(rev)).cloned().unwrap_or_default();
    let manifest = served.commit(rev, &manifest_of, &edited);
    stored.entry(manifest).or_insert(storage);
    manifest_of.push(manifest);
  }
  (served, stored)
}

impl Served {
  /// Commits generated revision `rev`: its file revisions, its manifest (made or reused) and its changeset's text.
  fn commit(&mut self, rev: Rev, manifest_of: &[Node], edited: &[(u8, u8)]) -> Node {
    let [p1, p2] = self.history.parents(rev);
    let base = |parent: Option<Rev>| parent.map_or(NULL_NODE, |parent| manifest_of[at(parent)]);
    let (m1, m2) = (base(p1), base(p2));
    let link = self.history.revs[at(rev)].node;
    let mut entries = self.entries(&m1);
    entries.extend(self.entries(&m2));
    let listed = self.edit(link, edited, &mut entries);
    let manifest = if listed.is_empty() && p2.is_none() && p1.is_some() {
      m1
    } else {
      let text = manifest_text(&entries);
      let manifest = node::compute(&m1, &m2, &text).unwrap();
      keep_made(&mut self.manifests, manifest, [m1, m2], link, text);
      manifest
    };
    let files: Vec<u8> = listed
      .iter()
      .flat_map(|path| [*path, b"\n"].concat())
      .collect();
    let text = [&hex(&manifest)[..], b"\ntest\n0 0\n", &files, b"\nchange"].concat();
    self.changesets.insert(link, text);
    manifest
  }

  /// Makes each edit's file revision over the one `entries` holds at its path, the first edit of a path alone; the
  /// paths edited, in order.
  fn edit(
    &mut self,
    link: Node,
    edited: &[(u8, u8)],
    entries: &mut BTreeMap<Vec<u8>, Node>,
  ) -> BTreeSet<&'static [u8]> {
    let mut listed = BTreeSet::new();
    for (path, content) in edited {
      let path = PATHS[usize::from(*path) % PATHS.len()];
      if listed.insert(path) {
        let parent = entries.get(path).copied().unwrap_or(NULL_NODE);
        let text = [path, &[b' ', *content][..]].concat();
        let file = node::compute(&parent, &NULL_NODE, &text).unwrap();
        let log = self.files.entry(path.to_vec()).or_default();
        keep_made(log, file, [parent, NULL_NODE], link, text);
        entries.insert(path.to_vec(), file);
      }
    }
    listed
  }

  /// A changeset's manifest node and the paths it lists.
  fn read(&self, link: &Node) -> (Node, Vec<Vec<u8>>) {
    let read = changeset::decode(&self.changesets[link]).unwrap();
    let files = read.file_names().map(<[u8]>::to_vec).collect();
    (read.manifest_node().unwrap(), files)
  }

  /// The entries 7.2 reads from a manifest's delta (`read_delta_parents(exact=False)`): those that differ from the
  /// parent it is stored against, or every one where it is stored whole or has no parent.
  fn delta_entries(
    &self,
    stored: &HashMap<Node, u8>,
    manifest_node: &Node,
  ) -> BTreeMap<Vec<u8>, Node> {
    let kept = &self.manifests[manifest_node];
    let against = match (stored[manifest_node], kept.parents) {
      (2, _) => NULL_NODE,
      (1, [_, p2]) if p2 != NULL_NODE => p2,
      (_, [p1, _]) => p1,
    };
    let base = self.entries(&against);
    let mut entries = self.entries(manifest_node);
    entries.retain(|path, node| base.get(path) != Some(node));
    entries
  }
}

/// A manifest's text for `entries`.
fn manifest_text(entries: &BTreeMap<Vec<u8>, Node>) -> Vec<u8> {
  let lines: Vec<manifest::Entry<'_>> = entries
    .iter()
    .map(|(path, node)| manifest::Entry {
      path,
      node: *node,
      flag: Flag::Regular,
    })
    .collect();
  let mut text = Vec::new();
  manifest::encode(&lines, &mut text).unwrap();
  text
}

/// Keeps a revision a generated changeset made in its log at the next position, once: the first maker its link.
fn keep_made(log: &mut Log, node: Node, parents: [Node; 2], link: Node, text: Vec<u8>) {
  let position = u64::try_from(log.len()).unwrap();
  log.entry(node).or_insert(Kept {
    position,
    parents,
    link,
    flags: 0,
    text,
  });
}

/// Each manifest the missing changesets name, linked to the first (`lookupcl`'s `setdefault`), and every path they
/// list (`changedfiles`).
fn first_namers(served: &Served, missing: &[Rev]) -> Namers {
  let mut namers = Linked::new();
  let mut changed = BTreeSet::new();
  for rev in missing {
    let link = served.history.revs[at(*rev)].node;
    let (manifest_node, files) = served.read(&link);
    if !namers.iter().any(|(known, _)| *known == manifest_node) {
      namers.push((manifest_node, link));
    }
    changed.extend(files);
  }
  (namers, changed)
}

/// Each file revision the sent manifests' deltas yield, by path, linked to the earliest missing changeset that yields
/// it (`lookupmflinknode`'s `fclnodes`).
fn yielded(
  served: &Served,
  stored: &HashMap<Node, u8>,
  namers: &[(Node, Node)],
  missing: &[Rev],
) -> Yielded {
  let order: HashMap<Node, usize> = missing
    .iter()
    .enumerate()
    .map(|(index, rev)| (served.history.revs[at(*rev)].node, index))
    .collect();
  let mut links = Yielded::new();
  for (manifest_node, link) in namers {
    for (path, file) in served.delta_entries(stored, manifest_node) {
      let earliest = links.entry(path).or_default().entry(file).or_insert(*link);
      if order[link] < order[&*earliest] {
        *earliest = *link;
      }
    }
  }
  links
}

/// What 7.2's packer sends of the manifest and file logs (`cgpacker.generate`'s slow path, stated literally): every
/// manifest the missing changesets name, in the log's order; then at each path a missing changeset lists, the file
/// revisions the manifests' deltas yield there whose file log's link is not common, in the log's order.
fn packed(
  served: &Served,
  stored: &HashMap<Node, u8>,
  missing: &[Rev],
  held: &BTreeSet<Rev>,
) -> Picked {
  let (mut namers, changed) = first_namers(served, missing);
  let links = yielded(served, stored, &namers, missing);
  namers.sort_by_key(|(manifest_node, _)| served.manifests[manifest_node].position);
  let not_common = |link: &Node| {
    served
      .history
      .rev(link)
      .is_none_or(|rev| !held.contains(&rev))
  };
  let files = changed
    .into_iter()
    .filter_map(|path| {
      let log = &served.files[&path];
      let mut sent: Linked = links
        .get(&path)?
        .iter()
        .filter(|(file, _)| not_common(&log[*file].link))
        .map(|(file, link)| (*file, *link))
        .collect();
      sent.sort_by_key(|(file, _)| log[file].position);
      (!sent.is_empty()).then_some((path, sent))
    })
    .collect();
  Picked {
    manifests: namers,
    files,
  }
}

/// What this module's changegroup carries of the manifest and file logs.
fn picked(changegroup: &Changegroup<'_>) -> Picked {
  let pairs = |revisions: &[Revision<'_>]| {
    revisions
      .iter()
      .map(|revision| (revision.node, revision.link))
      .collect()
  };
  Picked {
    manifests: pairs(changegroup.group(&changegroup.manifests)),
    files: changegroup
      .files
      .iter()
      .map(|group| {
        (
          group.name.to_vec(),
          pairs(changegroup.group(&group.revisions)),
        )
      })
      .collect(),
  }
}

/// `picked` without the file revisions the client holds: those a common changeset's manifest names.
fn lacking(mut picked: Picked, served: &Served, held: &BTreeSet<Rev>) -> Picked {
  let mut holds: BTreeSet<(Vec<u8>, Node)> = BTreeSet::new();
  for rev in held {
    let (manifest_node, _) = served.read(&served.history.revs[at(*rev)].node);
    holds.extend(served.entries(&manifest_node));
  }
  for (path, revisions) in &mut picked.files {
    revisions.retain(|(file, _)| !holds.contains(&(path.clone(), *file)));
  }
  picked.files.retain(|(_, revisions)| !revisions.is_empty());
  picked
}

/// Both packings of the changesets missing between picked heads and common revisions of a filled history: this
/// module's, then the literal packer's, each without what the client holds.
fn both_packings(
  history: History,
  edits: &[Edit],
  heads: &[u32],
  common: &[u32],
) -> (Picked, Picked) {
  let count = history.count();
  let pick = |picks: &[u32]| -> BTreeSet<Rev> { picks.iter().map(|pick| pick % count).collect() };
  let (mut heads, common) = (pick(heads), pick(common));
  if heads.is_empty() {
    heads.insert(count - 1);
  }
  let (served, stored) = filled(history, edits);
  let held = ancestors(&served.history, &common.iter().copied().collect::<Vec<_>>());
  let heads: Vec<Rev> = heads.into_iter().collect();
  let missing: Vec<Rev> = ancestors(&served.history, &heads)
    .difference(&held)
    .copied()
    .collect();
  let outgoing = Outgoing {
    heads,
    common: common.into_iter().collect(),
    missing: missing.clone(),
  };
  let ours = changegroup(&served, &outgoing, Version::V03, &mut room()).unwrap();
  let ours = picked(&Changegroup::decode(&ours, Version::V03, &mut room()).unwrap());
  let oracle = packed(&served, &stored, &missing, &held);
  (
    lacking(ours, &served, &held),
    lacking(oracle, &served, &held),
  )
}

/// A history of public changesets on `default` whose parents are `parents`.
fn shaped(parents: &[[Option<Rev>; 2]]) -> History {
  let mut history = History::default();
  for (seed, parents) in (1..).zip(parents) {
    history.push(Changeset {
      node: node_for(seed),
      parents: *parents,
      branch: b"default".to_vec(),
      closes: false,
      phase: Phase::Public,
    });
  }
  history
}

/// Each group's path and how many revisions it carries.
fn counted(picked: &Picked) -> Vec<(&[u8], usize)> {
  picked
    .files
    .iter()
    .map(|(path, revisions)| (path.as_slice(), revisions.len()))
    .collect()
}

/// A-619 (the model's non-vacuity): do pack, for a client holding changesets 0 and 2, changesets 3 and 4 of a history
/// where 1 and 2 both make `b/c`'s first revision (one file revision, its log linked to 1, on a branch neither held
/// nor sent), 3 edits `a` with its manifest stored whole, and 4 edits `b/c` again. Expect 7.2's packer to send `b/c`'s
/// first revision once more (its log's link is not common, and 3's whole manifest yields it), this module to leave it
/// out, both to send `a`'s and `b/c`'s new revisions and both manifests, and the two equal once what the client holds
/// is set aside: the divergence the model's comparison is built to allow, reached.
#[test]
fn mercurials_packer_resends_a_held_revision_this_module_leaves_out() {
  let history = shaped(&[
    [None, None],
    [Some(0), None],
    [Some(0), None],
    [Some(2), None],
    [Some(3), None],
  ]);
  let edits: Vec<Edit> = vec![
    (vec![(0, 0)], 0),
    (vec![(1, 0)], 0),
    (vec![(1, 0)], 0),
    (vec![(0, 1)], 2),
    (vec![(1, 1)], 0),
  ];
  let (served, stored) = filled(history, &edits);
  let held = ancestors(&served.history, &[2]);
  let missing: Vec<Rev> = vec![3, 4];
  let oracle = packed(&served, &stored, &missing, &held);
  let outgoing = Outgoing {
    heads: vec![4],
    common: vec![2],
    missing: missing.clone(),
  };
  let ours = changegroup(&served, &outgoing, Version::V03, &mut room()).unwrap();
  let ours = picked(&Changegroup::decode(&ours, Version::V03, &mut room()).unwrap());
  assert_eq!(counted(&oracle), [(&b"a"[..], 1), (&b"b/c"[..], 2)]);
  assert_eq!(
    (counted(&ours), ours.manifests.len()),
    (vec![(&b"a"[..], 1), (&b"b/c"[..], 1)], 2)
  );
  assert_eq!(
    lacking(ours, &served, &held),
    lacking(oracle, &served, &held)
  );
}

/// The refusal `getbundle` gives `asked`, if any.
fn refusal(served: &Served, asked: &Asked<'_>) -> Option<Refusal> {
  getbundle(served, asked, &mut room(), &mut Vec::new()).err()
}

/// A part `getbundle` sent: its type, its parameters' values in order, and its payload.
type Sent = (Vec<u8>, Vec<Vec<u8>>, Vec<u8>);

/// The parts `getbundle` sends for `asked`.
fn parts(served: &Served, asked: &Asked<'_>) -> Vec<Sent> {
  let mut stream = Vec::new();
  getbundle(served, asked, &mut room(), &mut stream).unwrap();
  let bundle = Bundle2::decode(&stream, &mut room()).unwrap();
  bundle
    .top_level()
    .map(|(index, part)| {
      let values = part
        .params
        .iter()
        .map(|param| param.value.to_vec())
        .collect();
      (
        part.kind.to_vec(),
        values,
        bundle.payload_bytes(index, &mut room()).unwrap(),
      )
    })
    .collect()
}

/// The store the clone asked, rebuilt from 7.2's answer.
fn cloned() -> Served {
  let [session, ..] = SESSIONS;
  let mut served = Served {
    history: captured(session.state, session.bookmarks),
    ..Served::default()
  };
  let requests = requests(session.client);
  let answers = answers(session.server, &requests);
  let at = requests
    .iter()
    .position(|(command, _)| *command == b"getbundle")
    .unwrap();
  served.take(answers[at]);
  served
}

/// A `getbundle` capability list: `HG20`, then the bundle2 capability blob `blob`, quoted.
fn caps(blob: &[u8]) -> Vec<u8> {
  let mut caps = b"HG20,bundle2=".to_vec();
  capabilities::encode_bundle2_value(blob, &mut caps);
  caps
}

/// Format: the bundle2 capabilities a 7.2 client sends that this module reads.
const CLIENT_BLOB: &[u8] =
  b"HG20\nbookmarks\nchangegroup=01,02,03\nlistkeys\nobsmarkers=V0,V1\nphases=heads";

/// AC-HG.9 (hostile input), A-619: do ask for a bundle1 (no `HG2` capability; one only inside `bundle2=`), a
/// changegroup in version 01 alone or with no version listed, and bookmarks, phase heads and markers of a client that
/// cannot read them. Expect each refused by kind: a bundle1 as not served, the rest naming what the client and server
/// do not share, as 7.2 aborts on each.
#[test]
fn a_bundle1_and_parts_the_client_cannot_read_are_refused_by_kind() {
  let served = cloned();
  let (versions_01, versions_none) = (caps(b"HG20\nchangegroup=01"), caps(b"HG20\nbookmarks"));
  let readers = caps(b"HG20\nchangegroup=03");
  let shared = |what| Some(Refusal::NoCommonExchange { what });
  let yes = Some(&b"1"[..]);
  for (asked, expected) in [
    (Asked::default(), Some(Refusal::NoBundle2)),
    (
      Asked {
        bundlecaps: &readers[5..],
        ..Asked::default()
      },
      Some(Refusal::NoBundle2),
    ),
    (
      Asked {
        bundlecaps: &versions_01,
        ..Asked::default()
      },
      shared("changegroup version"),
    ),
    (
      Asked {
        bundlecaps: &versions_none,
        ..Asked::default()
      },
      shared("changegroup version"),
    ),
    (
      Asked {
        bundlecaps: &readers,
        bookmarks: yes,
        ..Asked::default()
      },
      shared("bookmarks"),
    ),
    (
      Asked {
        bundlecaps: &readers,
        phases: yes,
        ..Asked::default()
      },
      shared("phases"),
    ),
    (
      Asked {
        bundlecaps: &readers,
        obsmarkers: yes,
        ..Asked::default()
      },
      shared("obsolescence marker format"),
    ),
  ] {
    assert_eq!(refusal(&served, &asked), expected);
  }
}

/// AC-HG.9, A-619: do ask a client that names `HG20` and no bundle2 capability for the default parts, then with the
/// bookmarks' keys; and ask for nothing missing, with every head common, of a client that lists only version 01.
/// Expect an empty stream, then the one `LISTKEYS` part (7.2 sends no changegroup to such a client); and the version
/// still refused, as 7.2 chooses the version before it looks for what is missing.
#[test]
fn a_client_without_bundle2_capabilities_gets_no_changegroup() {
  let served = cloned();
  let bare = Asked {
    bundlecaps: b"HG20",
    ..Asked::default()
  };
  assert_eq!(parts(&served, &bare), Vec::<Sent>::new());
  let keys = parts(
    &served,
    &Asked {
      listkeys: Some(&b"bookmarks"[..]),
      ..bare
    },
  );
  let kinds: Vec<&[u8]> = keys.iter().map(|(kind, _, _)| kind.as_slice()).collect();
  assert_eq!(kinds, [&b"LISTKEYS"[..]]);
  let tips = head_revs(&served, &mut room()).unwrap();
  let heads = joined(tips.iter().map(|rev| served.history.revs[at(*rev)].node));
  let old = caps(b"HG20\nchangegroup=01");
  let nothing = Asked {
    bundlecaps: &old,
    heads: &heads,
    common: &heads,
    ..Asked::default()
  };
  assert_eq!(
    refusal(&served, &nothing),
    Some(Refusal::NoCommonExchange {
      what: "changegroup version"
    })
  );
}

/// AC-HG.9, A-619: do ask for `listkeys` absent, empty, and with an empty item before `phases`. Expect no part, then
/// one part for the empty namespace, then two: an empty namespace listing nothing and the phases, as 7.2 splits a
/// `csv` argument.
#[test]
fn listkeys_is_split_as_mercurial_splits_it() {
  let served = cloned();
  let blob = caps(CLIENT_BLOB);
  let namespaces = |listkeys: Option<&[u8]>| -> Vec<Vec<Vec<u8>>> {
    let asked = Asked {
      bundlecaps: &blob,
      cg: Some(&b"0"[..]),
      listkeys,
      ..Asked::default()
    };
    parts(&served, &asked)
      .into_iter()
      .map(|(_, values, _)| values)
      .collect()
  };
  assert_eq!(namespaces(None), Vec::<Vec<Vec<u8>>>::new());
  assert_eq!(namespaces(Some(&b""[..])), [vec![Vec::new()]]);
  let split = namespaces(Some(&b",phases"[..]));
  assert_eq!(split, [vec![Vec::new()], vec![b"phases".to_vec()]]);
}

/// AC-HG.9, A-619: do clone from a store that lost a changeset's text, then one that lost a manifest revision, then
/// one that lost a file revision. Expect each refused naming what it lost.
#[test]
fn a_store_that_lost_a_text_is_refused_by_kind() {
  let blob = caps(CLIENT_BLOB);
  let asked = Asked {
    bundlecaps: &blob,
    ..Asked::default()
  };
  let lost = |lose: fn(&mut Served)| {
    let mut served = cloned();
    lose(&mut served);
    refusal(&served, &asked)
  };
  let store = |what| Some(Refusal::Store { what });
  assert_eq!(
    lost(|served| served.changesets.clear()),
    store("a changeset's text")
  );
  assert_eq!(
    lost(|served| served.manifests.clear()),
    store("a manifest revision")
  );
  assert_eq!(
    lost(|served| served.files.clear()),
    store("a file revision")
  );
}

/// AC-HG.9, A-619: do clone with every admission from nothing up to the answer's own size. Expect each refused past
/// its admission until the answer fits, and none to panic.
#[test]
fn a_short_admission_is_refused_past_it() {
  let served = cloned();
  let blob = caps(CLIENT_BLOB);
  let asked = Asked {
    bundlecaps: &blob,
    phases: Some(&b"1"[..]),
    ..Asked::default()
  };
  let fits = (0..1 << 22)
    .step_by(256)
    .find(|room| getbundle(&served, &asked, &mut Admission::new(*room), &mut Vec::new()).is_ok())
    .unwrap();
  assert!(fits > 0, "nothing answers within no room");
  for room in (0..fits).step_by(256) {
    let refused = getbundle(&served, &asked, &mut Admission::new(room), &mut Vec::new());
    assert!(matches!(refused, Err(Refusal::PastAdmission(_))), "{room}");
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// AC-HG.2 (model), A-619: do fill generated histories with files (branches editing one path alike, merges taking a
  /// side's revision unlisted, empty changesets reusing their parent's manifest, each manifest's delta stored against
  /// a parent or whole) and pack the changesets missing between generated heads and common revisions. Expect every
  /// manifest as 7.2's packer, stated literally, sends it, and every file revision the client lacks too, with its
  /// link, in its log's order.
  #[test]
  fn the_changegroup_carries_what_mercurials_packer_carries(
    history in history(),
    edits in proptest::collection::vec(
      (proptest::collection::vec((0..3u8, 0..2u8), 0..3), 0..3u8),
      48,
    ),
    heads in proptest::collection::vec(any::<u32>(), 0..3),
    common in proptest::collection::vec(any::<u32>(), 0..3),
  ) {
    prop_assume!(history.count() > 0);
    let (ours, oracle) = both_packings(history, &edits, &heads, &common);
    prop_assert_eq!(ours, oracle);
  }

  /// AC-HG.9 (hostile input), A-619 part 3: do feed the session arbitrary bytes, step after step. Expect each step to
  /// ask for more than it holds, or to take between one byte and all of it, until the session ends or the input runs
  /// out: ended cleanly only by an empty line (one byte), ended by a refusal only with the rest taken; never a panic.
  #[test]
  fn arbitrary_session_input_never_panics(input in proptest::collection::vec(any::<u8>(), 0..300)) {
    let served = cloned();
    let mut rest = input.as_slice();
    while let (Read::Done(done, used), _, _) = step(&served, rest, 1 << 16) {
      prop_assert!(used > 0 && used <= rest.len());
      match done {
        Step::Ended { refused: None } => prop_assert!(used == 1 && rest.first() == Some(&b'\n')),
        Step::Ended { refused: Some(_) } => prop_assert_eq!(used, rest.len()),
        Step::Answered { .. } => {}
      }
      rest = rest.get(used..).unwrap_or_default();
      if matches!(done, Step::Ended { .. }) {
        break;
      }
    }
  }

  /// AC-HG.9 (hostile input), A-619: do ask the clone's store for arbitrary heads, common nodes, capabilities, booleans
  /// and namespaces. Expect a bundle2 stream or a typed refusal, never a panic, within a bounded admission.
  #[test]
  fn arbitrary_getbundle_arguments_never_panic(
    fields in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..64), 8),
    flags in any::<u8>(),
  ) {
    let field = |at: usize| fields.get(at).map_or(&[][..], Vec::as_slice);
    let maybe = |bit: u8, at: usize| (flags & bit != 0).then(|| field(at));
    let mut bundlecaps = caps(CLIENT_BLOB);
    bundlecaps.extend_from_slice(field(2));
    let asked = Asked {
      heads: field(0),
      common: field(1),
      bundlecaps: &bundlecaps,
      cg: maybe(1, 3),
      phases: maybe(2, 4),
      bookmarks: maybe(4, 5),
      obsmarkers: maybe(8, 6),
      listkeys: maybe(16, 7),
    };
    let mut out = Vec::new();
    if getbundle(&cloned(), &asked, &mut Admission::new(1 << 20), &mut out).is_ok() {
      prop_assert!(Bundle2::decode(&out, &mut room()).is_ok());
    }
  }
}
