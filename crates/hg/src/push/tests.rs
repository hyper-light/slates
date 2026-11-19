//! A-641's tests: a push's bundle read part by part, its race checks judged, and its reply written, held to what stock
//! Mercurial 7.2 sent and answered (`fixtures/push`, made by `make-push-fixtures.sh`), to 7.2's rules stated naively
//! as an oracle over generated histories, and to hostile bundles.

use proptest::prelude::*;

use super::check::{self, CheckRefusal, HeadsArgument, Moment};
use super::reply::{self, Refused, Replied, Reply};
use super::{Asks, BookmarkAt, Fault, Refusal, Unbundle, read};
use crate::bundle2::{Bundle2, Event, Param, Part};
use crate::changegroup::Version;
use crate::fixtures::node;
use crate::phases::{Phase, PhaseHead};
use crate::serve::{Repository, Rev};
use crate::{Admission, NULL_NODE, Node, bookmarks, heads, phases, pushkey};

/// One push stock 7.2 made: its name, the bundle its client sent, the server's reply, and the server's changesets and
/// bookmarks just before it took the push.
struct Push {
  /// The fixture's name.
  name: &'static str,
  /// The bundle the client sent.
  request: &'static [u8],
  /// The server's reply.
  reply: &'static [u8],
  /// The server's changesets: `rev node p1 p2 phase branch` a line.
  server: &'static str,
  /// The server's bookmarks: `name node` a line.
  bookmarks: &'static str,
}

/// The push fixture `$name`.
macro_rules! push {
  ($name:literal) => {
    Push {
      name: $name,
      request: include_bytes!(concat!("../../fixtures/push/", $name, ".hg")),
      reply: include_bytes!(concat!("../../fixtures/push/", $name, "-reply.hg")),
      server: include_str!(concat!("../../fixtures/push/", $name, "-server.txt")),
      bookmarks: include_str!(concat!("../../fixtures/push/", $name, "-bookmarks.txt")),
    }
  };
}

/// Every push the fixtures hold.
fn pushes() -> Vec<Push> {
  vec![
    push!("push-bookmark"),
    push!("push-phase"),
    push!("push-plain"),
    push!("push-race"),
    push!("push-bookmark-plain"),
    push!("push-race-bookmark"),
    push!("push-phase-plain"),
    push!("push-race-phase"),
    push!("push-newhead"),
    push!("push-unsupported-part"),
    push!("push-unsupported-param"),
  ]
}

/// The fixture `name`.
fn fixture(name: &str) -> Push {
  pushes().into_iter().find(|push| push.name == name).unwrap()
}

/// Room enough for anything a fixture holds.
fn room() -> Admission {
  Admission::new(u64::MAX)
}

/// A served view a test builds: changesets in revision order, and bookmarks.
#[derive(Clone, Debug, Default)]
struct View {
  /// The changesets.
  changesets: Vec<Changeset>,
  /// The bookmarks: each name and the node it names.
  bookmarks: Vec<(Vec<u8>, Node)>,
}

/// A changeset of a [`View`].
#[derive(Clone, Debug)]
struct Changeset {
  /// Its node.
  node: Node,
  /// Its parents.
  parents: [Option<Rev>; 2],
  /// Its named branch.
  branch: Vec<u8>,
  /// Whether it closes its branch.
  closes: bool,
  /// Its phase.
  phase: Phase,
}

/// A revision as an index.
fn at(rev: Rev) -> usize {
  usize::try_from(rev).unwrap()
}

impl View {
  /// The changeset of `rev`.
  fn changeset(&self, rev: Rev) -> Option<&Changeset> {
    self.changesets.get(usize::try_from(rev).ok()?)
  }

  /// The view a fixture's listings describe.
  fn of(push: &Push) -> View {
    let mut view = View::default();
    for line in push.server.lines() {
      let fields: Vec<&str> = line.split(' ').collect();
      let parent = |hex: &str| {
        let parent = node(hex);
        (parent != NULL_NODE).then(|| view.rev(&parent).unwrap())
      };
      let parents = [parent(fields[2]), parent(fields[3])];
      view.changesets.push(Changeset {
        node: node(fields[1]),
        parents,
        branch: fields[5].as_bytes().to_vec(),
        closes: false,
        phase: if fields[4] == "public" {
          Phase::Public
        } else {
          Phase::Draft
        },
      });
    }
    for line in push.bookmarks.lines() {
      let (name, hex) = line.split_once(' ').unwrap();
      view.bookmarks.push((name.as_bytes().to_vec(), node(hex)));
    }
    view
  }
}

impl Repository for View {
  fn count(&self) -> Rev {
    Rev::try_from(self.changesets.len()).unwrap()
  }

  fn node(&self, rev: Rev) -> Option<Node> {
    self.changeset(rev).map(|changeset| changeset.node)
  }

  fn rev(&self, node: &Node) -> Option<Rev> {
    let position = self
      .changesets
      .iter()
      .position(|changeset| changeset.node == *node)?;
    Rev::try_from(position).ok()
  }

  fn parents(&self, rev: Rev) -> [Option<Rev>; 2] {
    self
      .changeset(rev)
      .map_or([None, None], |changeset| changeset.parents)
  }

  fn branch(&self, rev: Rev) -> &[u8] {
    self
      .changeset(rev)
      .map_or(b"default".as_slice(), |changeset| &changeset.branch)
  }

  fn closes(&self, rev: Rev) -> bool {
    self
      .changeset(rev)
      .is_some_and(|changeset| changeset.closes)
  }

  fn phase(&self, rev: Rev) -> Phase {
    self
      .changeset(rev)
      .map_or(Phase::Public, |changeset| changeset.phase)
  }

  fn publishing(&self) -> bool {
    false
  }

  fn bookmarks(&self) -> impl Iterator<Item = (&[u8], Node)> {
    self
      .bookmarks
      .iter()
      .map(|(name, node)| (name.as_slice(), *node))
  }
}

/// A fixture's request, read.
fn read_push(push: &Push) -> Result<Unbundle, Refusal> {
  let bundle = Bundle2::decode(push.request, &mut room()).unwrap();
  read(&bundle, &mut room())
}

/// What each part a reply stream holds means: its type as sent, its parameters as sent (the mandatory first) with how
/// many are mandatory, and its payload.
type Meaning = Vec<(Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>, usize, Vec<u8>)>;

/// What `stream` means, part by part.
fn meaning(stream: &[u8]) -> Meaning {
  let bundle = Bundle2::decode(stream, &mut room()).unwrap();
  bundle
    .parts
    .iter()
    .enumerate()
    .map(|(index, part)| {
      let params = part
        .params
        .iter()
        .map(|param| (param.key.to_vec(), param.value.to_vec()))
        .collect();
      let payload = bundle.payload_bytes(index, &mut room()).unwrap();
      (part.kind.to_vec(), params, part.mandatory_params, payload)
    })
    .collect()
}

/// `reply`, written.
fn written(reply: &Reply) -> Vec<u8> {
  let mut out = Vec::new();
  reply.encode(&mut out).unwrap();
  assert_eq!(
    reply.encoded_len().unwrap(),
    u64::try_from(out.len()).unwrap()
  );
  out
}

/// Holds `reply` to the fixture's reply: the same parts, parameters and payloads, and the same bytes.
fn assert_replies_as_7_2(reply: &Reply, push: &Push) {
  let ours = written(reply);
  assert_eq!(meaning(&ours), meaning(push.reply), "{}", push.name);
  assert_eq!(ours, push.reply, "{}", push.name);
}

/// The parts' types a read push took, in order.
fn kinds(unbundle: &Unbundle) -> Vec<&'static str> {
  unbundle
    .parts
    .iter()
    .map(|taken| match taken.asks {
      Asks::ReplyCaps(_) => "replycaps",
      Asks::CheckHeads(_) => "check:heads",
      Asks::CheckUpdatedHeads(_) => "check:updated-heads",
      Asks::CheckPhases(_) => "check:phases",
      Asks::CheckBookmarks(_) => "check:bookmarks",
      Asks::Changegroup(_) => "changegroup",
      Asks::PhaseHeads(_) => "phase-heads",
      Asks::Bookmarks(_) => "bookmarks",
      Asks::Pushkey(_) => "pushkey",
      Asks::Obsmarkers(_) => "obsmarkers",
      Asks::Pushvars(_) => "pushvars",
      Asks::Output => "output",
    })
    .collect()
}

/// A-641, AC-HG.9 (golden). Do: read every push stock 7.2's client sent. Expect: the parts its listing shows, in its
/// order, each a part its handler takes; the unknown mandatory part and parameter refused as 7.2's server refused
/// them.
#[test]
fn each_push_7_2_sent_is_read_part_by_part_as_its_listing_shows() {
  let plain = [
    "replycaps",
    "check:phases",
    "check:updated-heads",
    "changegroup",
  ];
  let marked = [
    "replycaps",
    "check:bookmarks",
    "check:phases",
    "check:updated-heads",
    "changegroup",
    "bookmarks",
  ];
  let expected: [(&str, &[&str]); 9] = [
    (
      "push-bookmark",
      &["replycaps", "check:bookmarks", "check:phases", "bookmarks"],
    ),
    ("push-phase", &["replycaps", "check:phases", "phase-heads"]),
    ("push-plain", &plain),
    ("push-race", &plain),
    ("push-phase-plain", &plain),
    ("push-race-phase", &plain),
    ("push-bookmark-plain", &marked),
    ("push-race-bookmark", &marked),
    ("push-newhead", &["replycaps", "changegroup"]),
  ];
  for (name, parts) in expected {
    assert_eq!(kinds(&read_push(&fixture(name)).unwrap()), parts, "{name}");
  }
  assert_eq!(
    read_push(&fixture("push-unsupported-part")),
    Err(Refusal::Unsupported {
      parttype: b"test:unknown".to_vec(),
      params: Vec::new(),
    })
  );
  assert_eq!(
    read_push(&fixture("push-unsupported-param")),
    Err(Refusal::Unsupported {
      parttype: b"changegroup".to_vec(),
      params: vec![b"slates-unknown".to_vec()],
    })
  );
}

/// A-641 (golden). Do: read the bookmark move and the publication. Expect: the bookmark checked at the node the server
/// held and set to the one the client moved it to; the published draft's node as a public head.
#[test]
fn a_bookmark_move_and_a_publication_ask_what_the_client_moved() {
  let bookmark = fixture("push-bookmark");
  let view = View::of(&bookmark);
  let unbundle = read_push(&bookmark).unwrap();
  let main = view.bookmarks[0].1;
  let moved = view.changesets[1].node;
  assert!(unbundle.parts.iter().any(|taken| taken.asks
    == Asks::CheckBookmarks(vec![BookmarkAt {
      name: b"main".to_vec(),
      node: Some(main),
    }])));
  assert!(unbundle.parts.iter().any(|taken| taken.asks
    == Asks::Bookmarks(vec![BookmarkAt {
      name: b"main".to_vec(),
      node: Some(moved),
    }])));
  let phase = fixture("push-phase");
  let published = View::of(&phase).changesets[1].node;
  assert!(
    read_push(&phase)
      .unwrap()
      .parts
      .iter()
      .any(|taken| taken.asks
        == Asks::PhaseHeads(vec![PhaseHead {
          phase: Phase::Public,
          node: published,
        }]))
  );
  let (id, part) = read_push(&fixture("push-plain"))
    .unwrap()
    .changegroup()
    .map(|(id, part)| (id, part.clone()))
    .unwrap();
  assert_eq!(
    (id, part.version, part.target_phase),
    (3, Version::V03, None)
  );
}

/// A-641, AC-HG.3 (golden). Do: judge each push's checks against the server as it stood when the push arrived. Expect:
/// the three pushes sent again after another client moved a head, a bookmark or a phase raced, each in exactly the
/// words 7.2's server answered with; every other push's checks hold.
#[test]
fn each_race_7_2_found_is_found_in_its_words_and_no_other() {
  for push in pushes() {
    let Ok(unbundle) = read_push(&push) else {
      continue;
    };
    let judged = check::judge(&unbundle, &View::of(&push), &mut room());
    let answered = meaning(push.reply);
    let raced = answered
      .iter()
      .find(|(kind, ..)| kind.eq_ignore_ascii_case(b"error:pushraced"));
    match raced {
      Some((_, params, ..)) => assert_eq!(
        judged,
        Err(CheckRefusal::Raced {
          words: params[0].1.clone()
        }),
        "{}",
        push.name
      ),
      None => assert_eq!(judged, Ok(()), "{}", push.name),
    }
  }
}

/// A-641 (golden). Do: write the reply to each push stock 7.2's server answered. Expect: the same parts, parameters and
/// payloads, and the same bytes: `reply:changegroup` naming the changegroup part with 1 (heads unchanged), an empty
/// stream for a bookmark or a phase alone, `error:pushraced` with the race's words, `error:abort` in
/// `reject_new_heads`' words, `error:unsupportedcontent` naming the part and the parameter.
#[test]
fn each_reply_is_written_as_7_2s_server_wrote_it() {
  for push in pushes() {
    let reply = match read_push(&push) {
      Err(Refusal::Unsupported { parttype, params }) => Reply::refused(
        &[],
        &Refused::Unsupported {
          parttype: Some(parttype),
          params,
        },
      ),
      Err(other) => panic!("{}: {other:?}", push.name),
      Ok(unbundle) => reply_to(&push, &unbundle),
    };
    assert_replies_as_7_2(&reply, &push);
  }
}

/// The reply to a push the fixtures hold that was read: its race, the new head 7.2's hook refused, or its acceptance
/// (one head replaced by one).
fn reply_to(push: &Push, unbundle: &Unbundle) -> Reply {
  if let Err(CheckRefusal::Raced { words }) = check::judge(unbundle, &View::of(push), &mut room()) {
    return Reply::refused(&[], &Refused::PushRaced { message: words });
  }
  if push.name == "push-newhead" {
    let message = reply::multiple_heads(b"default");
    return Reply::refused(
      &[],
      &Refused::Abort {
        message,
        hint: None,
      },
    );
  }
  let replied: Vec<Replied> = unbundle
    .changegroup()
    .map(|(id, _)| Replied::Changegroup {
      in_reply_to: id,
      heads: reply::heads_changed(1, 1, 0),
    })
    .into_iter()
    .collect();
  Reply::accepted(&replied, unbundle.reply_caps().is_some())
}

/// A-641 (`changegroup.apply`). Do: count the change in heads for pushes that keep, add, remove and close heads.
/// Expect: 7.2's numbers, never 0: heads unchanged 1, n added n + 1, n removed −n − 1, an added head that closes its
/// branch not counted.
#[test]
fn the_change_in_heads_is_counted_as_7_2_counts_it_and_is_never_zero() {
  let cases: [((u64, u64, u64), i64); 6] = [
    ((0, 0, 0), 1),
    ((1, 1, 0), 1),
    ((0, 2, 0), 3),
    ((1, 0, 0), -2),
    ((0, 1, 1), 1),
    ((u64::MAX, 0, 0), i64::MIN),
  ];
  for ((removed, added, closing), expected) in cases {
    assert_eq!(reply::heads_changed(removed, added, closing), expected);
  }
}

/// A-641 (`hooklib.reject_new_heads`, CPython 3.12's bytes repr). Do: write the new-head refusal's words for branch
/// names with quotes, escapes, control bytes and UTF-8. Expect: each exactly as 7.2 under CPython 3.12 wrote it
/// (`_(b"Changes on branch %r resulted in multiple heads") % pycompat.bytestr(name)`, run on the Mac Studio
/// 2026-11-19 through `uvx --python 3.12 --from mercurial==7.2 python -I`).
#[test]
fn the_new_head_refusal_names_its_branch_as_7_2_does() {
  let cases: [(&[u8], &str); 10] = [
    (b"default", "'default'"),
    (b"it's", "\"it's\""),
    (b"say \"hi\"", "'say \"hi\"'"),
    (b"both ' and \"", "'both \\' and \"'"),
    (b"tab\there", "'tab\\there'"),
    (b"nul\x00", "'nul\\x00'"),
    (b"\x7f\x80\xff", "'\\x7f\\x80\\xff'"),
    (b"back\\slash", "'back\\\\slash'"),
    (b"new\nline\r", "'new\\nline\\r'"),
    ("caf\u{e9}".as_bytes(), "'caf\\xc3\\xa9'"),
  ];
  for (branch, repr) in cases {
    let words = format!("Changes on branch {repr} resulted in multiple heads");
    assert_eq!(reply::multiple_heads(branch), words.as_bytes());
  }
}

/// One part of a stream a test builds by hand.
struct Spec {
  /// Its type as sent.
  kind: &'static [u8],
  /// Its mandatory parameters.
  mandatory: Vec<(&'static [u8], &'static [u8])>,
  /// Its advisory parameters.
  advisory: Vec<(&'static [u8], &'static [u8])>,
  /// Its payload's chunks.
  chunks: Vec<&'static [u8]>,
}

/// A parameter, its key and value.
fn kv(key: &'static [u8], value: &'static [u8]) -> (&'static [u8], &'static [u8]) {
  (key, value)
}

/// A part of `kind` with no parameter and `chunks` for its payload.
fn spec(kind: &'static [u8], chunks: &[&'static [u8]]) -> Spec {
  Spec {
    kind,
    mandatory: Vec::new(),
    advisory: Vec::new(),
    chunks: chunks.to_vec(),
  }
}

/// The bundle2 stream of `specs`, their ids their positions.
fn stream(specs: &[Spec]) -> Vec<u8> {
  let mut parts = Vec::new();
  let mut events = Vec::new();
  for (index, spec) in specs.iter().enumerate() {
    let params = spec
      .mandatory
      .iter()
      .chain(&spec.advisory)
      .map(|(key, value)| Param { key, value })
      .collect();
    parts.push(Part {
      kind: spec.kind,
      id: u32::try_from(index).unwrap(),
      params,
      mandatory_params: spec.mandatory.len(),
      depth: 0,
    });
    events.push(Event::Open(index));
    events.extend(spec.chunks.iter().map(|chunk| Event::Chunk(chunk)));
    events.push(Event::Close);
  }
  let mut out = Vec::new();
  Bundle2 {
    params: Vec::new(),
    parts,
    events,
  }
  .encode(&mut out)
  .unwrap();
  out
}

/// `specs`' stream, read.
fn read_specs(specs: &[Spec]) -> Result<Unbundle, Refusal> {
  let bytes = stream(specs);
  read(&Bundle2::decode(&bytes, &mut room()).unwrap(), &mut room())
}

/// A-641, AC-HG.9 (`bundle2._gethandler`). Do: send parts no push handler takes and parameters a handler does not name,
/// mandatory and advisory. Expect: a mandatory unknown part refused naming its type in lower case; a known mandatory
/// part's unknown mandatory parameters refused, sorted; an advisory unknown part, an advisory part with an unknown
/// mandatory parameter and an unknown advisory parameter passed over; a pull's part and a reply's part unknown here.
#[test]
fn unknown_parts_and_parameters_are_refused_when_mandatory_and_passed_over_when_advisory() {
  let unsupported = |parttype: &[u8]| {
    Err(Refusal::Unsupported {
      parttype: parttype.to_vec(),
      params: Vec::new(),
    })
  };
  assert_eq!(
    read_specs(&[spec(b"Slates:Odd", &[])]),
    unsupported(b"slates:odd")
  );
  assert_eq!(
    read_specs(&[spec(b"LISTKEYS", &[])]),
    unsupported(b"listkeys")
  );
  assert_eq!(
    read_specs(&[spec(b"ERROR:ABORT", &[])]),
    unsupported(b"error:abort")
  );
  let mut odd = spec(b"CHECK:HEADS", &[]);
  odd.mandatory = vec![kv(b"zz", b"1"), kv(b"aa", b"2")];
  let sorted = vec![b"aa".to_vec(), b"zz".to_vec()];
  assert_eq!(
    read_specs(&[odd]),
    Err(Refusal::Unsupported {
      parttype: b"check:heads".to_vec(),
      params: sorted
    })
  );
  let mut advisory_odd = spec(b"check:heads", &[]);
  advisory_odd.mandatory = vec![kv(b"zz", b"1")];
  let sevens: &'static [u8] = &[7; 20];
  let mut advisory_param = spec(b"CHECK:HEADS", &[sevens]);
  advisory_param.advisory = vec![kv(b"slates-hint", b"x")];
  let taken = read_specs(&[spec(b"slates:odd", &[]), advisory_odd, advisory_param]).unwrap();
  assert_eq!(kinds(&taken), ["check:heads"]);
  assert_eq!(taken.parts[0].asks, Asks::CheckHeads(vec![[7; 20]]));
  assert_eq!((taken.parts[0].id, taken.parts[0].mandatory), (2, true));
}

/// A-641, AC-HG.9. Do: send each payload a push's handlers decode, damaged. Expect: each refused by kind, naming its
/// part: a heads list not whole nodes, a phase 7.2 does not define, a bookmark cut short, a pushkey without its new
/// value.
#[test]
fn a_damaged_payload_is_refused_by_kind_naming_its_part() {
  let payload = |id: u32, fault: Fault| Err(Refusal::Payload { id, fault });
  let nineteen: &'static [u8] = &[1; 19];
  assert_eq!(
    read_specs(&[spec(b"CHECK:UPDATED-HEADS", &[nineteen])]),
    payload(0, Fault::Heads(heads::Refusal::PartialNode { len: 19 }))
  );
  let unknown_phase: &'static [u8] = &[
    0, 0, 0, 7, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
  ];
  assert_eq!(
    read_specs(&[spec(b"PHASE-HEADS", &[unknown_phase])]),
    payload(
      0,
      Fault::Phases(phases::Refusal::UnknownPhase { number: 7, at: 0 })
    )
  );
  let short_entry: &'static [u8] = &[2; 21];
  let cut = read_specs(&[spec(b"output", &[]), spec(b"BOOKMARKS", &[short_entry])]);
  assert!(matches!(
    cut,
    Err(Refusal::Payload {
      id: 1,
      fault: Fault::Bookmarks(bookmarks::Refusal::Truncated(_))
    })
  ));
  let mut half = spec(b"PUSHKEY", &[]);
  half.mandatory = vec![
    kv(b"namespace", b"bookmarks"),
    kv(b"key", b"main"),
    kv(b"old", b""),
  ];
  assert_eq!(
    read_specs(&[half]),
    payload(
      0,
      Fault::Pushkey(pushkey::Refusal::MissingParameter { key: b"new" })
    )
  );
}

/// A changegroup part with `mandatory` parameters and an empty payload.
fn changegroup(mandatory: Vec<(&'static [u8], &'static [u8])>) -> Spec {
  let mut part = spec(b"CHANGEGROUP", &[b"cg".as_slice()]);
  part.mandatory = mandatory;
  part
}

/// A-641, §4.17.3 "The wire surface" and decision 3. Do: send changegroup parts in each version, twice, with numbers
/// that are none and with each target phase. Expect: versions 01 to 03 taken (01 when none is named), 04, 05 and a
/// version 7.2 lacks refused; a second changegroup refused; a `nbchanges` or `targetphase` that is no number refused; a
/// target phase of public or draft taken and secret, archived or internal refused.
#[test]
fn a_changegroup_is_taken_once_in_versions_01_to_03_and_never_secret() {
  let named: [(Option<&'static [u8]>, Version); 3] = [
    (None, Version::V01),
    (Some(b"02"), Version::V02),
    (Some(b"03"), Version::V03),
  ];
  for (named, version) in named {
    let params = named
      .map(|value| kv(b"version", value))
      .into_iter()
      .collect();
    let taken = read_specs(&[changegroup(params)]).unwrap();
    assert_eq!(taken.changegroup().unwrap().1.version, version);
  }
  let refused: [&'static [u8]; 4] = [b"04", b"05", b"06", b""];
  for named in refused {
    assert!(matches!(
      read_specs(&[changegroup(vec![kv(b"version", named)])]),
      Err(Refusal::ChangegroupVersion { id: 0, .. })
    ));
  }
  assert_eq!(
    read_specs(&[changegroup(Vec::new()), changegroup(Vec::new())]),
    Err(Refusal::SecondChangegroup { id: 1 })
  );
  let keys: [&'static [u8]; 2] = [b"nbchanges", b"targetphase"];
  for key in keys {
    assert!(matches!(
      read_specs(&[changegroup(vec![kv(key, b"x")])]),
      Err(Refusal::Parameter { id: 0, .. })
    ));
  }
  let taken = read_specs(&[changegroup(vec![
    kv(b"targetphase", b"1"),
    kv(b"nbchanges", b"12"),
  ])])
  .unwrap();
  let part = taken.changegroup().unwrap().1;
  assert_eq!(
    (part.target_phase, part.changesets),
    (Some(Phase::Draft), Some(12))
  );
  let secret: [&'static [u8]; 3] = [b"2", b"32", b"96"];
  for number in secret {
    assert!(matches!(
      read_specs(&[changegroup(vec![kv(b"targetphase", number)])]),
      Err(Refusal::TargetPhase { id: 0, .. })
    ));
  }
}

/// A-641, AC-HG.9. Do: interrupt an advisory part with a mandatory one. Expect: refused as unsupported content naming
/// the interrupting part's type: no push carries an interruption.
#[test]
fn a_mandatory_part_inside_another_is_refused() {
  let parts = vec![
    Part {
      kind: b"output",
      id: 0,
      params: Vec::new(),
      mandatory_params: 0,
      depth: 0,
    },
    Part {
      kind: b"TEST:NESTED",
      id: 1,
      params: Vec::new(),
      mandatory_params: 0,
      depth: 1,
    },
  ];
  let events = vec![
    Event::Open(0),
    Event::Chunk(b"words"),
    Event::Interrupt,
    Event::Open(1),
    Event::Close,
    Event::Close,
  ];
  let mut bytes = Vec::new();
  Bundle2 {
    params: Vec::new(),
    parts,
    events,
  }
  .encode(&mut bytes)
  .unwrap();
  let bundle = Bundle2::decode(&bytes, &mut room()).unwrap();
  assert_eq!(
    read(&bundle, &mut room()),
    Err(Refusal::Unsupported {
      parttype: b"test:nested".to_vec(),
      params: Vec::new(),
    })
  );
}

/// A-641, §4.9. Do: read every push stock 7.2 sent with exactly the room it needs, and with less. Expect: taken with
/// exactly that room, and refused past the admission with any less, nothing panicking.
#[test]
fn a_push_is_read_within_exactly_the_room_it_needs() {
  for push in pushes() {
    let bundle = Bundle2::decode(push.request, &mut room()).unwrap();
    let mut measured = room();
    if read(&bundle, &mut measured).is_err() {
      continue;
    }
    let needed = u64::MAX - measured.left();
    assert!(
      read(&bundle, &mut Admission::new(needed)).is_ok(),
      "{}",
      push.name
    );
    for less in [0, needed / 2, needed - 1] {
      assert!(
        matches!(
          read(&bundle, &mut Admission::new(less)),
          Err(Refusal::PastAdmission(_))
        ),
        "{} with {less}",
        push.name
      );
    }
  }
}

/// A-641 (`exchange.check_heads`). Do: read and judge `unbundle`'s `heads` argument in each form against a two-headed
/// repository and an empty one. Expect: `force` holds; the heads in `heads`' order hold and in another order race; the
/// digest of the sorted heads holds and any other races; the null node alone holds for the empty repository; a race
/// in 7.2's words for the moment; an item not hexadecimal refused.
#[test]
fn the_heads_argument_holds_as_check_heads_judges_it() {
  // Shape: a root and two children of it, so the heads are the two children, the newer first.
  let shapes: [Shape; 3] = [
    (0, 0, 0, false, false),
    (1, 0, 0, false, false),
    (2, 0, 0, false, false),
  ];
  let view = generated(&shapes, &[]);
  let hex = |nodes: &[Node]| {
    let items: Vec<String> = nodes.iter().map(|node| hexed(node)).collect();
    items.join(" ")
  };
  let judge = |value: &str, view: &View, moment| {
    let argument = check::heads_argument(value.as_bytes(), &mut room()).unwrap();
    check::judge_heads_argument(&argument, view, moment, &mut room())
  };
  let [first, second] = [view.changesets[1].node, view.changesets[2].node];
  assert_eq!(
    check::heads_argument(b"666f726365", &mut room()),
    Ok(HeadsArgument::Force)
  );
  assert_eq!(judge("666f726365", &view, Moment::Uploading), Ok(()));
  assert_eq!(
    judge(&hex(&[second, first]), &view, Moment::Uploading),
    Ok(())
  );
  let reordered = judge(&hex(&[first, second]), &view, Moment::Preparing);
  let words = b"repository changed while preparing changes - please try again".to_vec();
  assert_eq!(reordered, Err(CheckRefusal::Raced { words }));
  let mut joined = Vec::new();
  let mut sorted = [first, second];
  sorted.sort_unstable();
  sorted
    .iter()
    .for_each(|node| joined.extend_from_slice(node));
  let digest = crate::node::sha1(&joined).unwrap();
  let hashed = format!("686173686564 {}", hexed(&digest));
  assert_eq!(judge(&hashed, &view, Moment::Uploading), Ok(()));
  let wrong = format!("686173686564 {}", hexed(&first));
  let words = b"repository changed while uploading changes - please try again".to_vec();
  assert_eq!(
    judge(&wrong, &view, Moment::Uploading),
    Err(CheckRefusal::Raced { words })
  );
  assert_eq!(
    judge(&hex(&[NULL_NODE]), &View::default(), Moment::Uploading),
    Ok(())
  );
  assert!(judge("", &View::default(), Moment::Uploading).is_err());
  assert_eq!(
    check::heads_argument(b"66 zz", &mut room()),
    Err(check::HeadsArgumentRefusal::NotHex { at: 1 })
  );
}

/// A node in lower-case hexadecimal.
fn hexed(node: &Node) -> String {
  node.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 7.2's `short`, stated: the first twelve hexadecimal digits.
fn short_of(node: &Node) -> String {
  hexed(node).chars().take(12).collect()
}

/// The oracle's own statement of 7.2's race words (`handlecheckheads`).
const RACE_WORDS: &str = "remote repository changed while pushing - please try again";

/// The oracle's statement of 7.2's four checks (`bundle2_part_handlers.py`), naively, over a [`View`].
fn oracle(view: &View, checks: &[Asks]) -> Result<(), Vec<u8>> {
  for asked in checks {
    match asked {
      Asks::CheckHeads(claimed) => oracle_heads(view, claimed)?,
      Asks::CheckUpdatedHeads(claimed) => oracle_updated(view, claimed)?,
      Asks::CheckBookmarks(claimed) => oracle_bookmarks(view, claimed)?,
      Asks::CheckPhases(claimed) => oracle_phases(view, claimed)?,
      _ => {}
    }
  }
  Ok(())
}

/// The revisions no revision names as a parent.
fn naive_heads(view: &View) -> Vec<Node> {
  let parents: Vec<Rev> = view
    .changesets
    .iter()
    .flat_map(|c| c.parents.into_iter().flatten())
    .collect();
  (0..view.count())
    .filter(|rev| !parents.contains(rev))
    .map(|rev| view.changesets[at(rev)].node)
    .collect()
}

/// `check:heads`: `sorted(heads) != sorted(repo.heads())`, an empty repository's heads `[nullid]`.
fn oracle_heads(view: &View, claimed: &[Node]) -> Result<(), Vec<u8>> {
  let mut held = naive_heads(view);
  if held.is_empty() {
    held.push(NULL_NODE);
  }
  let mut claimed = claimed.to_vec();
  held.sort();
  claimed.sort();
  if held == claimed {
    Ok(())
  } else {
    Err(RACE_WORDS.as_bytes().to_vec())
  }
}

/// `check:updated-heads`: every node a head of its branch by 7.2's rule (A-667, `branchmap.py`: no later changeset on
/// its branch descends from it, whatever branches the path crosses), closed ones too; `[nullid]` only when empty.
fn oracle_updated(view: &View, claimed: &[Node]) -> Result<(), Vec<u8>> {
  if claimed == [NULL_NODE] {
    return if view.changesets.is_empty() {
      Ok(())
    } else {
      Err(RACE_WORDS.as_bytes().to_vec())
    };
  }
  let is_branch_head = |node: &Node| {
    view.changesets.iter().enumerate().any(|(rev, changeset)| {
      changeset.node == *node
        && !view
          .changesets
          .iter()
          .enumerate()
          .skip(rev + 1)
          .any(|(later, child)| child.branch == changeset.branch && descends(view, later, rev))
    })
  };
  if claimed.iter().all(is_branch_head) {
    Ok(())
  } else {
    Err(RACE_WORDS.as_bytes().to_vec())
  }
}

/// Whether changeset `later` has changeset `rev` among its ancestors, every path walked.
fn descends(view: &View, later: usize, rev: usize) -> bool {
  let mut seen = vec![false; view.changesets.len()];
  let mut stack = vec![later];
  while let Some(next) = stack.pop() {
    if next == rev {
      return true;
    }
    if !std::mem::replace(&mut seen[next], true) {
      stack.extend(view.changesets[next].parents.iter().flatten().map(|parent| at(*parent)));
    }
  }
  false
}

/// `check:bookmarks`: 7.2's three messages.
fn oracle_bookmarks(view: &View, claimed: &[BookmarkAt]) -> Result<(), Vec<u8>> {
  for mark in claimed {
    let now = view
      .bookmarks
      .iter()
      .rev()
      .find(|(name, _)| *name == mark.name)
      .map(|(_, node)| *node);
    let book = String::from_utf8(mark.name.clone()).unwrap();
    let raced = RACE_WORDS;
    let words = match (mark.node, now) {
      (saw, now) if saw == now => continue,
      (None, Some(now)) => format!(
        "{raced} (bookmark \"{book}\" set on {}, expected missing)",
        short_of(&now)
      ),
      (Some(saw), None) => format!(
        "{raced} (bookmark \"{book}\" is missing, expected {})",
        short_of(&saw)
      ),
      (Some(saw), Some(now)) => {
        format!(
          "{raced} (bookmark \"{book}\" move from {} to {})",
          short_of(&saw),
          short_of(&now)
        )
      }
      (None, None) => continue,
    };
    return Err(words.into_bytes());
  }
  Ok(())
}

/// `check:phases`: phase by phase in `allphases` order, each phase's nodes in the payload's order.
fn oracle_phases(view: &View, claimed: &[PhaseHead]) -> Result<(), Vec<u8>> {
  for phase in [
    Phase::Public,
    Phase::Draft,
    Phase::Secret,
    Phase::Archived,
    Phase::Internal,
  ] {
    for entry in claimed.iter().filter(|entry| entry.phase == phase) {
      let actual = if entry.node == NULL_NODE {
        Phase::Public
      } else {
        match view.changesets.iter().find(|c| c.node == entry.node) {
          Some(changeset) => changeset.phase,
          None => return Err(RACE_WORDS.as_bytes().to_vec()),
        }
      };
      if actual != phase {
        let raced = RACE_WORDS;
        let words = format!(
          "{raced} ({} is {} expected {})",
          short_of(&entry.node),
          actual.name(),
          phase.name()
        );
        return Err(words.into_bytes());
      }
    }
  }
  Ok(())
}

/// Format: the branches a generated history's changesets are on.
const BRANCHES: [&[u8]; 3] = [b"default", b"stable", b"other"];

/// Format: the bookmarks a generated history may hold.
const MARKS: [&[u8]; 3] = [b"main", b"@", b"feature"];

/// A changeset a generated history holds: its two parents' picks, its branch, whether it closes it, whether it is a
/// draft.
type Shape = (u8, u8, u8, bool, bool);

/// A generated check: which check, its node picks, whether each bookmark pick names none, and each phase pick.
type Claim = (u8, Vec<(u8, bool, u8)>);

/// The history `shapes` and `marks` describe: each changeset's parents picked among the ones before it, a draft's
/// children drafts, and each bookmark at a picked changeset.
fn generated(shapes: &[Shape], marks: &[(u8, u8)]) -> View {
  let mut view = View::default();
  for (rev, (first, second, branch, closes, draft)) in shapes.iter().enumerate() {
    let pick = |choice: u8| {
      (rev > 0 && choice % 4 != 0).then(|| Rev::try_from(usize::from(choice) % rev).unwrap())
    };
    let p1 = pick(*first);
    let p2 = pick(*second).filter(|p2| p1.is_some() && Some(*p2) != p1);
    let inherited = [p1, p2]
      .iter()
      .flatten()
      .any(|parent| view.changesets[at(*parent)].phase == Phase::Draft);
    view.changesets.push(Changeset {
      node: [u8::try_from(rev + 1).unwrap(); 20],
      parents: [p1, p2],
      branch: BRANCHES[usize::from(*branch) % BRANCHES.len()].to_vec(),
      closes: *closes,
      phase: if *draft || inherited {
        Phase::Draft
      } else {
        Phase::Public
      },
    });
  }
  if !view.changesets.is_empty() {
    for (mark, pick) in marks {
      let name = MARKS[usize::from(*mark) % MARKS.len()].to_vec();
      let node = view.changesets[usize::from(*pick) % view.changesets.len()].node;
      view.bookmarks.retain(|(held, _)| *held != name);
      view.bookmarks.push((name, node));
    }
  }
  view
}

/// The node a pick names: one of the history's, one it lacks, or the null node.
fn picked(view: &View, pick: u8) -> Node {
  match usize::from(pick) % (view.changesets.len() + 2) {
    index if index < view.changesets.len() => view.changesets[index].node,
    index if index == view.changesets.len() => NULL_NODE,
    _ => [0xee; 20],
  }
}

/// The check a generated claim makes; `check:heads` names the history's own heads when its first pick is even, so the
/// held case is met.
fn claim(view: &View, (which, picks): &Claim) -> Asks {
  let nodes = picks.iter().map(|(pick, ..)| picked(view, *pick)).collect();
  match which % 4 {
    0 if picks.first().is_some_and(|(pick, ..)| pick % 2 == 0) => {
      Asks::CheckHeads(naive_heads(view))
    }
    0 => Asks::CheckHeads(nodes),
    1 => Asks::CheckUpdatedHeads(nodes),
    2 => Asks::CheckBookmarks(
      picks
        .iter()
        .map(|(pick, none, mark)| BookmarkAt {
          name: MARKS[usize::from(*mark) % MARKS.len()].to_vec(),
          node: (!none).then(|| picked(view, *pick)),
        })
        .collect(),
    ),
    _ => Asks::CheckPhases(
      picks
        .iter()
        .map(|(pick, _, phase)| PhaseHead {
          phase: [Phase::Public, Phase::Draft, Phase::Secret][usize::from(*phase) % 3],
          node: picked(view, *pick),
        })
        .collect(),
    ),
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-641, AC-HG.3 (T-HG.3, the serial model of lines, bookmarks and phases). Do: judge generated checks, one to four
  /// of every kind, against generated histories of up to eight changesets on three branches with drafts and
  /// bookmarks. Expect: exactly the oracle's verdict and words, 7.2's rules stated naively.
  #[test]
  fn the_checks_judge_as_7_2s_rules_stated_naively(
    shapes in prop::collection::vec(any::<Shape>(), 0..8),
    marks in prop::collection::vec(any::<(u8, u8)>(), 0..4),
    claims in prop::collection::vec((any::<u8>(), prop::collection::vec(any::<(u8, bool, u8)>(), 0..4)), 1..5),
  ) {
    let view = generated(&shapes, &marks);
    let checks: Vec<Asks> = claims.iter().map(|asked| claim(&view, asked)).collect();
    let unbundle = Unbundle {
      parts: checks.iter().cloned().enumerate().map(|(id, asks)| super::Taken {
        id: u32::try_from(id).unwrap_or(u32::MAX),
        mandatory: true,
        asks,
      }).collect(),
    };
    let judged = check::judge(&unbundle, &view, &mut room());
    let expected = oracle(&view, &checks).map_err(|words| CheckRefusal::Raced { words });
    prop_assert_eq!(judged, expected);
  }

  /// A-641, AC-HG.9 (T-HG.9). Do: flip one bit of, or cut, a push stock 7.2 sent, anywhere, then decode, read and judge
  /// it. Expect: each step answers or refuses by kind, and nothing panics.
  #[test]
  fn a_stock_push_flipped_or_cut_anywhere_is_read_or_refused(
    which in 0_usize..11,
    place in any::<prop::sample::Index>(),
    bit in 0_u8..8,
    cut in any::<bool>(),
  ) {
    let pushes = pushes();
    let Some(push) = pushes.get(which) else {
      return Ok(());
    };
    let mut bytes = push.request.to_vec();
    let place = place.index(bytes.len());
    if cut {
      bytes.truncate(place);
    } else if let Some(byte) = bytes.get_mut(place) {
      *byte ^= 1 << bit;
    }
    if let Ok(bundle) = Bundle2::decode(&bytes, &mut room())
      && let Ok(unbundle) = read(&bundle, &mut room())
    {
      let judged = check::judge(&unbundle, &View::of(push), &mut room());
      let answered = matches!(judged, Ok(()) | Err(CheckRefusal::Raced { .. }));
      prop_assert!(answered, "{:?}", judged);
    }
  }
}
