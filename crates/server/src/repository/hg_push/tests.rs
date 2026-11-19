//! A-657's server tests: Mercurial pushes landed on a daemon's owning shard through A-635's append, held to what stock
//! 7.2 answered (`crates/hg/fixtures/push`, made by `make-push-fixtures.sh`): the fixtures' repository rebuilt from its
//! texts in a new Mercurial repository (every node 7.2's), then each stock push landed on it and its reply compared
//! byte for byte with 7.2's.

use std::collections::BTreeSet;

use proptest::prelude::*;
use slates_db::catalog::{Principal, RepositoryId, StateLog, VolumeId as DbVolumeId};
use slates_hg::bundle2::Bundle2;
use slates_hg::changegroup::{Changegroup, Group, Revision, Version};
use slates_hg::index::{HgIndex, Log};
use slates_hg::phases::{Phase, PhaseHead};
use slates_hg::push::held::IndexHeld;
use slates_hg::push::reply::{Refused, Replied, Reply};
use slates_hg::push::{Asks, BookmarkAt, ChangegroupPart, Taken, Unbundle, read};
use slates_hg::serve::{Repository, Rev};
use slates_hg::{Admission, NULL_NODE, Node, delta, node};
use slates_ipc::protocol::{ObjectFormat, ReplyBody, RequestBody, VcsTool};
use slates_merge::push::{HEAD_ATTRIBUTE, ROOT};
use slates_vcs::ContentStore;

use super::{
  BOOKMARK_TAG, LINE_TAG, Landed, Prepared, bookmark_name, branch_prefix, land, line_name, line_of,
  prepare, transaction_id,
};
use crate::state::ShardState;
use crate::verbs::{dispatch, to_db_volume};

/// Shape: the room a test's work is built within, far past any of these trees.
const ROOM: u64 = 1 << 20;
/// Shape: the window the tests' content stores are made with, as slates-hg's import tests make theirs.
const WINDOW: usize = 1 << 12;
/// Shape: the push's name every test work's identity begins with.
const NAME: [u8; 32] = [7; 32];

/// A fixture file of `crates/hg/fixtures/push`.
macro_rules! fixture {
  ($name:literal) => {
    include_bytes!(concat!("../../../../hg/fixtures/push/", $name))
  };
}

/// A fixture listing of `crates/hg/fixtures/push`.
macro_rules! listing {
  ($name:literal) => {
    include_str!(concat!("../../../../hg/fixtures/push/", $name))
  };
}

fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// Room enough for anything a test holds.
fn room() -> Admission {
  Admission::new(u64::MAX)
}

/// A node's forty lower-case hexadecimal digits, as 7.2's `hex` writes them.
fn hex(node: &Node) -> String {
  crate::verbs::hex_of(node)
}

/// A node from its forty hexadecimal digits, as 7.2's `bin` reads them.
fn parsed(hex: &str) -> Node {
  slates_hg::pushkey::hex_node(hex.as_bytes())
    .unwrap()
    .unwrap()
}

/// A served view a test builds from a fixture's listings: `{rev} {node} {p1} {p2} {phase} {branch}` a line, and
/// `{name} {node}` a bookmark.
#[derive(Clone, Debug, Default)]
struct View {
  changesets: Vec<(Node, [Option<Rev>; 2], Vec<u8>, Phase)>,
  bookmarks: Vec<(Vec<u8>, Node)>,
}

impl View {
  fn of(server: &str, bookmarks: &str) -> View {
    let mut view = View::default();
    for line in server.lines() {
      let fields: Vec<&str> = line.split(' ').collect();
      let parent = |hex: &str| (parsed(hex) != NULL_NODE).then(|| view.rev(&parsed(hex)).unwrap());
      let parents = [parent(fields[2]), parent(fields[3])];
      let phase = if fields[4] == "public" {
        Phase::Public
      } else {
        Phase::Draft
      };
      view.changesets.push((
        parsed(fields[1]),
        parents,
        fields[5].as_bytes().to_vec(),
        phase,
      ));
    }
    for line in bookmarks.lines() {
      let (name, hex) = line.split_once(' ').unwrap();
      view.bookmarks.push((name.as_bytes().to_vec(), parsed(hex)));
    }
    view
  }

  fn at(&self, rev: Rev) -> &(Node, [Option<Rev>; 2], Vec<u8>, Phase) {
    &self.changesets[usize::try_from(rev).unwrap()]
  }
}

impl Repository for View {
  fn count(&self) -> Rev {
    Rev::try_from(self.changesets.len()).unwrap()
  }

  fn node(&self, rev: Rev) -> Option<Node> {
    self
      .changesets
      .get(usize::try_from(rev).ok()?)
      .map(|held| held.0)
  }

  fn rev(&self, node: &Node) -> Option<Rev> {
    let at = self.changesets.iter().position(|held| held.0 == *node)?;
    Rev::try_from(at).ok()
  }

  fn parents(&self, rev: Rev) -> [Option<Rev>; 2] {
    self.at(rev).1
  }

  fn branch(&self, rev: Rev) -> &[u8] {
    &self.at(rev).2
  }

  fn closes(&self, _rev: Rev) -> bool {
    false
  }

  fn phase(&self, rev: Rev) -> Phase {
    self.at(rev).3
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

/// A changeset of the fixtures' repository as `make-push-fixtures.sh` made it with stock 7.2 (user `test`, date `0
/// 0`): its description, the one file it adds and that file's bytes, its parent among those made before it, and its
/// branch.
type Commit = (
  &'static str,
  &'static str,
  &'static [u8],
  Option<usize>,
  &'static str,
);

/// `base`, `second`, then another client's `other` and the stock push's `plain`, each on `second`; then two changesets
/// of a `stable` branch this repository's tests add, the first on `second`.
const COMMITS: [Commit; 6] = [
  ("base", "a", b"a\n", None, "default"),
  ("second", "b", b"b\n", Some(0), "default"),
  ("other", "e", b"e\n", Some(1), "default"),
  ("plain", "d", b"d\n", Some(1), "default"),
  ("on stable", "s", b"s\n", Some(1), "stable"),
  ("stable again", "t", b"t\n", Some(4), "stable"),
];

/// One changeset rebuilt: its node, first parent and text; its manifest's node, first parent and text; its file's
/// path, node and text.
#[derive(Clone, Debug)]
struct Made {
  node: Node,
  p1: Node,
  text: Vec<u8>,
  manifest: (Node, Node, Vec<u8>),
  file: (Vec<u8>, Node, Vec<u8>),
}

/// The fixtures' repository rebuilt from its texts as 7.2's `commit` writes them (§4.17.3 decision 7's fields).
fn history() -> Vec<Made> {
  let mut made: Vec<Made> = Vec::new();
  let mut manifests: Vec<Vec<(Vec<u8>, Node)>> = Vec::new();
  for (description, path, bytes, parent, branch) in COMMITS {
    let file = node::compute(&NULL_NODE, &NULL_NODE, bytes).unwrap();
    let mut entries = parent.map(|at| manifests[at].clone()).unwrap_or_default();
    entries.push((path.as_bytes().to_vec(), file));
    entries.sort();
    let text: Vec<u8> = entries
      .iter()
      .flat_map(|(path, node)| [path.as_slice(), b"\0", hex(node).as_bytes(), b"\n"].concat())
      .collect();
    let parent_manifest = parent.map_or(NULL_NODE, |at| made[at].manifest.0);
    let manifest = node::compute(&parent_manifest, &NULL_NODE, &text).unwrap();
    let p1 = parent.map_or(NULL_NODE, |at| made[at].node);
    let extras = if branch == "default" {
      String::new()
    } else {
      format!(" branch:{branch}")
    };
    let changeset = format!(
      "{}\ntest\n0 0{extras}\n{path}\n\n{description}",
      hex(&manifest)
    )
    .into_bytes();
    made.push(Made {
      node: node::compute(&p1, &NULL_NODE, &changeset).unwrap(),
      p1,
      text: changeset,
      manifest: (manifest, parent_manifest, text),
      file: (path.as_bytes().to_vec(), file, bytes.to_vec()),
    });
    manifests.push(entries);
  }
  made
}

/// A revision sent as its full text against the null node, its second parent none.
fn full_text(node: Node, p1: Node, link: Node, data: &[u8]) -> Revision<'_> {
  Revision {
    node,
    p1,
    p2: NULL_NODE,
    base: NULL_NODE,
    link,
    flags: 0,
    info: None,
    protocol_flags: 0,
    data,
    sidedata: None,
  }
}

/// A changegroup 03 of the changesets `which`, every revision a full text against the null node.
fn changegroup(made: &[Made], which: &[usize]) -> ChangegroupPart {
  let mut deltas: Vec<Vec<u8>> = Vec::new();
  for at in which {
    for text in [&made[*at].text, &made[*at].manifest.2, &made[*at].file.2] {
      let mut encoded = Vec::new();
      delta::encode_full_text(text, &mut encoded).unwrap();
      deltas.push(encoded);
    }
  }
  let mut revisions = Vec::new();
  for (index, at) in which.iter().enumerate() {
    let made = &made[*at];
    revisions.push(full_text(made.node, made.p1, made.node, &deltas[3 * index]));
  }
  for (index, at) in which.iter().enumerate() {
    let made = &made[*at];
    revisions.push(full_text(
      made.manifest.0,
      made.manifest.1,
      made.node,
      &deltas[3 * index + 1],
    ));
  }
  let mut files = Vec::new();
  for (index, at) in which.iter().enumerate() {
    let made = &made[*at];
    let start = revisions.len();
    revisions.push(full_text(
      made.file.1,
      NULL_NODE,
      made.node,
      &deltas[3 * index + 2],
    ));
    files.push(Group {
      name: made.file.0.as_slice(),
      revisions: start..start + 1,
    });
  }
  let count = which.len();
  let group = Changegroup {
    version: Version::V03,
    revisions,
    changelog: 0..count,
    manifests: count..2 * count,
    trees: Vec::new(),
    files,
  };
  let mut payload = Vec::new();
  group.encode(&mut payload).unwrap();
  ChangegroupPart {
    version: Version::V03,
    changesets: None,
    target_phase: None,
    tree_manifests: false,
    payload,
  }
}

/// A mandatory part.
fn taken(id: u32, asks: Asks) -> Taken {
  Taken {
    id,
    mandatory: true,
    asks,
  }
}

/// A stock push as 7.2's client sent it.
fn stock(bytes: &[u8]) -> Unbundle {
  let bundle = Bundle2::decode(bytes, &mut room()).unwrap();
  read(&bundle, &mut room()).unwrap()
}

/// A reply's bytes.
fn encoded(reply: &Reply) -> Vec<u8> {
  let mut out = Vec::new();
  reply.encode(&mut out).unwrap();
  out
}

/// A Mercurial repository on the shard, with the index and store its pushes keep their revisions in.
struct Hg {
  repository: RepositoryId,
  root: DbVolumeId,
  index: HgIndex,
  store: ContentStore,
}

impl Hg {
  /// A new Mercurial repository `name`, its first line `default`'s, empty.
  fn new(state: &mut ShardState, name: &str) -> Hg {
    let reply = dispatch(
      state,
      1,
      &owner(),
      RequestBody::CreateRepository {
        name: name.to_owned(),
        tool: VcsTool::Mercurial,
        format: ObjectFormat::Sha1,
        default_branch: line_name(b"default", 0),
        green: None,
      },
    );
    let ReplyBody::GreenCreated { id } = reply else {
      panic!("no repository: {reply:?}")
    };
    let repository = state
      .db
      .partition()
      .repositories()
      .repository_named(name)
      .unwrap()
      .id;
    Hg {
      repository,
      root: to_db_volume(id),
      index: HgIndex::new(),
      store: ContentStore::new(1 << 24, WINDOW).unwrap(),
    }
  }

  fn prepare(
    &self,
    state: &ShardState,
    unbundle: &Unbundle,
    view: &View,
  ) -> Result<Prepared, Refused> {
    let held = IndexHeld::new(&self.index, &self.store);
    prepare(
      state,
      self.repository,
      unbundle,
      (view, &held),
      (false, NAME, ROOM),
      &mut room(),
    )
  }

  fn land(&mut self, state: &mut ShardState, prepared: Prepared) -> Landed {
    let landed = land(
      state,
      (&owner(), self.repository),
      prepared,
      (&mut self.index, &mut self.store),
      &mut room(),
    );
    assert_eq!(
      landed.released,
      Ok(()),
      "the index let go of was not given back"
    );
    landed
  }

  /// Prepares and lands `unbundle` read against `view`: its answer.
  fn push(
    &mut self,
    state: &mut ShardState,
    unbundle: &Unbundle,
    view: &View,
  ) -> Result<Reply, Refused> {
    let prepared = self.prepare(state, unbundle, view)?;
    self.land(state, prepared).answer
  }

  /// The green a name binds.
  fn bound(&self, state: &ShardState, name: &str) -> Option<DbVolumeId> {
    state
      .db
      .partition()
      .repositories()
      .branch(self.repository, name)
  }

  /// The draft roots the register holds.
  fn roots(&self, state: &ShardState) -> BTreeSet<Node> {
    state
      .db
      .partition()
      .repositories()
      .phase_roots(self.repository)
      .filter(|(phase, _)| *phase == 1)
      .map(|(_, node)| Node::try_from(node).unwrap())
      .collect()
  }

  /// The phases log's position.
  fn phases(&self, state: &ShardState) -> u64 {
    state
      .db
      .partition()
      .repositories()
      .log_position(self.repository, StateLog::Phases)
      .unwrap()
  }

  /// Whether the index holds changeset `node`.
  fn holds(&self, node: &Node) -> bool {
    self.index.rev(&self.store, Log::Changelog, node).is_some()
  }
}

/// A green's head, as its root's attribute holds it.
fn head(state: &ShardState, green: DbVolumeId) -> Option<Node> {
  state.greens.get(&green).and_then(|engine| {
    engine
      .xattr(ROOT, HEAD_ATTRIBUTE)
      .map(|bytes| Node::try_from(bytes).unwrap())
  })
}

/// A green's file at its head.
fn file(state: &ShardState, green: DbVolumeId, path: &str) -> Option<Vec<u8>> {
  state
    .greens
    .get(&green)
    .and_then(|engine| engine.content(path).map(<[u8]>::to_vec))
}

/// Rebuilds the fixtures' repository in `hg` as one push to its empty first line: `base` and `second`, the changesets
/// `public` names published by a `phase-heads` part, bookmark `main` made at `mark`.
fn rebuild(state: &mut ShardState, hg: &mut Hg, (public, mark): (&[usize], usize)) -> Vec<Made> {
  let made = history();
  let heads = public
    .iter()
    .map(|at| PhaseHead {
      phase: Phase::Public,
      node: made[*at].node,
    })
    .collect();
  let unbundle = Unbundle {
    parts: vec![
      taken(0, Asks::Changegroup(changegroup(&made, &[0, 1]))),
      taken(1, Asks::PhaseHeads(heads)),
      taken(
        2,
        Asks::Bookmarks(vec![BookmarkAt {
          name: b"main".to_vec(),
          node: Some(made[mark].node),
        }]),
      ),
    ],
  };
  let answer = hg.push(state, &unbundle, &View::default());
  assert!(answer.is_ok(), "the rebuild was refused: {answer:?}");
  made
}

/// The rebuilt repository against 7.2's: every node 7.2's, `default`'s first line at `second` holding both files, and
/// the index holding both changesets.
fn assert_rebuilt(state: &ShardState, hg: &Hg, made: &[Made]) {
  assert_eq!(
    hex(&made[0].node),
    "ad120869acf0c6c266f1293ca5da567d8b382f1d"
  );
  assert_eq!(
    hex(&made[1].node),
    "59d5dc23f8786c06d0fae53e625be1cdd640a783"
  );
  assert_eq!(
    hex(&made[1].manifest.0),
    "a539ce0c1a22b0ecf34498f9f5ce8ea56df9ecb7"
  );
  assert_eq!(
    hex(&made[2].node),
    "f2563910228049a7bcbc2f7ea79337803edcbcaa"
  );
  assert_eq!(hg.bound(state, &line_name(b"default", 0)), Some(hg.root));
  assert_eq!(head(state, hg.root), Some(made[1].node));
  assert_eq!(file(state, hg.root, "b").as_deref(), Some(&b"b\n"[..]));
  assert!(hg.holds(&made[0].node) && hg.holds(&made[1].node));
}

/// A-657, AC-HG.3 (T-HG.3; golden, by meaning and byte for byte). Do: rebuild the fixtures' repository from its texts
/// in a new Mercurial repository (`base` and `second` landed as one push to its empty first line, both published by a
/// `phase-heads` part, bookmark `main` made at `second`), then land stock 7.2's `push-plain`, then land it again
/// without its race checks (a client that heard no answer). Expect: the rebuilt nodes 7.2's; `main` a green of its
/// own at `second`, no draft root; `push-plain` answered byte for byte as 7.2 answered it, `default`'s line at its
/// changeset, which is a draft root and held by the index; sent again it is answered alike and records nothing.
#[test]
fn a_stock_push_lands_on_the_rebuilt_repository_as_7_2_answered_it() {
  crate::daemon::audit_on_shard(|state| {
    let mut hg = Hg::new(state, "plain");
    let made = rebuild(state, &mut hg, (&[1], 1));
    assert_rebuilt(state, &hg, &made);
    let main = hg.bound(state, &bookmark_name(b"main")).unwrap();
    assert_eq!(head(state, main), Some(made[1].node));
    assert!(hg.roots(state).is_empty());
    let view = View::of(
      listing!("push-plain-server.txt"),
      listing!("push-plain-bookmarks.txt"),
    );
    let mut unbundle = stock(fixture!("push-plain.hg"));
    let answer = hg.push(state, &unbundle, &view);
    assert_eq!(
      answer.map(|reply| encoded(&reply)),
      Ok(fixture!("push-plain-reply.hg").to_vec())
    );
    assert_plain_landed(state, &hg, &made);
    let version = state.db.partition().green_head(hg.root);
    unbundle
      .parts
      .retain(|part| !matches!(part.asks, Asks::CheckUpdatedHeads(_) | Asks::CheckPhases(_)));
    let mut after = view;
    after.changesets.push((
      made[3].node,
      [Some(1), None],
      b"default".to_vec(),
      Phase::Draft,
    ));
    let again = hg.push(state, &unbundle, &after);
    assert_eq!(
      again.map(|reply| encoded(&reply)),
      Ok(fixture!("push-plain-reply.hg").to_vec())
    );
    assert_eq!(
      state.db.partition().green_head(hg.root),
      version,
      "a push sent again recorded"
    );
  });
}

fn assert_plain_landed(state: &ShardState, hg: &Hg, made: &[Made]) {
  assert_eq!(
    hex(&made[3].node),
    "048741fabe422ed31728cd666c1e36b6c3f0a96d"
  );
  assert_eq!(head(state, hg.root), Some(made[3].node));
  assert_eq!(file(state, hg.root, "d").as_deref(), Some(&b"d\n"[..]));
  assert_eq!(hg.roots(state), BTreeSet::from([made[3].node]));
  assert!(hg.holds(&made[3].node));
}

/// A-657 part 4 (`HG_TXNID`; 7.2's `localrepo.transaction`). Do: prepare stock `push-plain` on the rebuilt repository,
/// and take the transaction ids of the push's name and of another. Expect: the prepared push's id is its name's, `TXN:`
/// and forty lower-case hexadecimal digits; the same name gives the same id, another name another.
#[test]
fn a_push_names_its_transaction_as_7_2_does() {
  crate::daemon::audit_on_shard(|state| {
    let mut hg = Hg::new(state, "transaction");
    rebuild(state, &mut hg, (&[1], 1));
    let view = View::of(
      listing!("push-plain-server.txt"),
      listing!("push-plain-bookmarks.txt"),
    );
    let prepared = hg
      .prepare(state, &stock(fixture!("push-plain.hg")), &view)
      .unwrap();
    assert_eq!(prepared.transaction, transaction_id(&NAME));
    let digits = prepared.transaction.strip_prefix("TXN:").unwrap();
    assert_eq!(digits.len(), 40);
    assert!(
      digits
        .bytes()
        .all(|digit| matches!(digit, b'0'..=b'9' | b'a'..=b'f'))
    );
    assert_eq!(transaction_id(&NAME), transaction_id(&[7; 32]));
    assert_ne!(transaction_id(&NAME), transaction_id(&[8; 32]));
  });
}

/// A reply part as a client reads it: its type, the part it answers (`in-reply-to`) and its payload.
type ReplyPart = (Vec<u8>, Option<Vec<u8>>, Vec<u8>);

/// A reply's parts as a client reads them.
fn reply_parts(bytes: &[u8]) -> Vec<ReplyPart> {
  let bundle = Bundle2::decode(bytes, &mut room()).unwrap();
  bundle
    .top_level()
    .map(|(index, part)| {
      let payload = bundle
        .payload(index)
        .flat_map(|chunk| chunk.iter().copied())
        .collect();
      let answers = part.param(b"in-reply-to").map(<[u8]>::to_vec);
      (part.kind.to_vec(), answers, payload)
    })
    .collect()
}

/// A-657 part 4 (§4.17.3 "Hooks"; 7.2's `bundle2._processpart` writes what a part's handler printed, a
/// `pretxnchangegroup` hook's words among it, as an `output` part right after that handler's reply, naming the part).
/// Do: prepare stock `push-plain` on the rebuilt repository, hand it the words an admission hook printed as `run_hooks`
/// hands them back (naming the push's changegroup part), and land it. Expect: its hooks are handed the push's one
/// changeset as first and last; the reply is 7.2's `push-plain` reply with the hook's words as an `output` part in
/// reply to the changegroup part, right after `reply:changegroup`.
#[test]
fn a_hooks_words_follow_the_changegroup_reply_naming_its_part() {
  crate::daemon::audit_on_shard(|state| {
    let mut hg = Hg::new(state, "words");
    let made = rebuild(state, &mut hg, (&[1], 1));
    let view = View::of(
      listing!("push-plain-server.txt"),
      listing!("push-plain-bookmarks.txt"),
    );
    let unbundle = stock(fixture!("push-plain.hg"));
    let (changegroup, _) = unbundle.changegroup().unwrap();
    let mut prepared = hg.prepare(state, &unbundle, &view).unwrap();
    assert_eq!(prepared.added, Some((made[3].node, made[3].node)));
    let words = b"pretxnchangegroup.words: checked\n".to_vec();
    let in_reply_to = prepared.changegroup;
    prepared.with_output(vec![Replied::Output {
      in_reply_to,
      words: words.clone(),
    }]);
    let reply = hg.land(state, prepared).answer.unwrap();
    let mut expected = reply_parts(fixture!("push-plain-reply.hg"));
    let after = expected
      .iter()
      .position(|(kind, ..)| kind.as_slice() == b"reply:changegroup")
      .unwrap();
    let named = changegroup.to_string().into_bytes();
    expected.insert(
      after.saturating_add(1),
      (b"output".to_vec(), Some(named), words),
    );
    assert_eq!(reply_parts(&encoded(&reply)), expected);
  });
}

/// A-657, A-642 (a race found where the entry appends). Do: on the rebuilt repository, prepare stock `push-plain`
/// (its check that `second` is a head holds), then land another client's push moving `default` (`other`), then land
/// the prepared push. Expect: the other push lands; the prepared one is refused as 7.2's race, byte for byte its
/// `push-race` reply, and nothing of it is applied: `default` at `other`, its changeset not held, no root of it.
#[test]
fn a_push_whose_line_moved_after_its_read_is_answered_as_7_2s_race() {
  crate::daemon::audit_on_shard(|state| {
    let mut hg = Hg::new(state, "race");
    let made = rebuild(state, &mut hg, (&[1], 1));
    let view = View::of(
      listing!("push-plain-server.txt"),
      listing!("push-plain-bookmarks.txt"),
    );
    let prepared = hg
      .prepare(state, &stock(fixture!("push-plain.hg")), &view)
      .unwrap();
    let other = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[2])))],
    };
    assert!(hg.push(state, &other, &view).is_ok());
    assert_eq!(head(state, hg.root), Some(made[2].node));
    let refused = hg.land(state, prepared).answer.map(|reply| encoded(&reply));
    let race = refused.map_err(|refused| encoded(&Reply::refused(&[], &refused)));
    assert_eq!(race, Err(fixture!("push-race-reply.hg").to_vec()));
    assert_eq!(head(state, hg.root), Some(made[2].node));
    assert!(!hg.holds(&made[3].node));
    assert_eq!(hg.roots(state), BTreeSet::from([made[2].node]));
  });
}

/// A view of the rebuilt repository's `base` and `second`, public, and the changesets `drafts` names after them, each
/// a draft on its commit's parent and branch.
fn view_with(made: &[Made], drafts: &[usize]) -> View {
  let mut view = View::of(
    listing!("push-plain-server.txt"),
    listing!("push-plain-bookmarks.txt"),
  );
  for at in drafts {
    let parent = COMMITS[*at]
      .3
      .and_then(|parent| view.rev(&made[parent].node));
    let branch = COMMITS[*at].4.as_bytes().to_vec();
    view
      .changesets
      .push((made[*at].node, [parent, None], branch, Phase::Draft));
  }
  view
}

/// A-657, A-642 (a check of a green the push does not move compares its position). Do: on the rebuilt repository, land
/// a first changeset on a new branch `stable` (a line made from `default`'s), then prepare a push of `plain` on
/// `default` carrying `check:heads` (the heads it read: `stable`'s changeset alone), then land another push moving
/// `stable`, then land the prepared push. Expect: `stable`'s line made, numbered 0; the prepared push's own line never
/// moved, so its verdict alone would accept it, but `stable`'s position no longer what it read, so it is refused as
/// 7.2's race, and nothing of it is applied.
#[test]
fn a_push_is_refused_when_a_green_it_only_checked_moved() {
  crate::daemon::audit_on_shard(|state| {
    let mut hg = Hg::new(state, "heads");
    let made = rebuild(state, &mut hg, (&[1], 1));
    let first = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[4])))],
    };
    assert!(hg.push(state, &first, &view_with(&made, &[])).is_ok());
    let stable = hg.bound(state, &line_name(b"stable", 0)).unwrap();
    assert_eq!(head(state, stable), Some(made[4].node));
    let checked = Unbundle {
      parts: vec![
        taken(0, Asks::CheckHeads(vec![made[4].node])),
        taken(1, Asks::Changegroup(changegroup(&made, &[3]))),
      ],
    };
    let prepared = hg
      .prepare(state, &checked, &view_with(&made, &[4]))
      .unwrap();
    let again = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[5])))],
    };
    assert!(hg.push(state, &again, &view_with(&made, &[4])).is_ok());
    assert_eq!(head(state, stable), Some(made[5].node));
    let raced = hg.land(state, prepared).answer;
    let words = super::RACED.to_vec();
    assert_eq!(raced, Err(Refused::PushRaced { message: words }));
    assert_eq!(head(state, hg.root), Some(made[1].node));
    assert!(!hg.holds(&made[3].node));
  });
}

/// A-657 (§4.17.3 decision 3: phases moved in the same entry). Do: rebuild with only `base` published and `main` at
/// `second`, then land stock `push-phase` (its `phase-heads` part publishes `second`). Expect: before it, `second` the
/// one draft root; it is answered byte for byte as 7.2 answered it (a stream with no part), the root removed, the
/// phases log one position on.
#[test]
fn a_published_draft_leaves_the_roots_in_the_same_entry() {
  crate::daemon::audit_on_shard(|state| {
    let mut hg = Hg::new(state, "phase");
    let made = rebuild(state, &mut hg, (&[0], 1));
    assert_eq!(hg.roots(state), BTreeSet::from([made[1].node]));
    let position = hg.phases(state);
    let view = View::of(
      listing!("push-phase-server.txt"),
      listing!("push-phase-bookmarks.txt"),
    );
    let answer = hg.push(state, &stock(fixture!("push-phase.hg")), &view);
    assert_eq!(
      answer.map(|reply| encoded(&reply)),
      Ok(fixture!("push-phase-reply.hg").to_vec())
    );
    assert!(hg.roots(state).is_empty());
    assert_eq!(hg.phases(state), position + 1);
  });
}

/// A-657 (§4.17.3 decision 2: a bookmark is a green). Do: rebuild with only `base` published and `main` at `base`,
/// then land stock `push-bookmark` (`check:bookmarks` and a `bookmarks` part moving `main` to `second`). Expect:
/// before it, `main` a green at `base` without `b`; it is answered byte for byte as 7.2 answered it, and `main`'s green
/// moved to `second`, holding `b`; `default`'s line untouched.
#[test]
fn a_bookmark_moves_on_its_own_green() {
  crate::daemon::audit_on_shard(|state| {
    let mut hg = Hg::new(state, "bookmark");
    let made = rebuild(state, &mut hg, (&[0], 0));
    let main = hg.bound(state, &bookmark_name(b"main")).unwrap();
    assert_eq!(head(state, main), Some(made[0].node));
    assert_eq!(file(state, main, "b"), None);
    let version = state.db.partition().green_head(hg.root);
    let view = View::of(
      listing!("push-bookmark-server.txt"),
      listing!("push-bookmark-bookmarks.txt"),
    );
    let answer = hg.push(state, &stock(fixture!("push-bookmark.hg")), &view);
    assert_eq!(
      answer.map(|reply| encoded(&reply)),
      Ok(fixture!("push-bookmark-reply.hg").to_vec())
    );
    assert_eq!(head(state, main), Some(made[1].node));
    assert_eq!(file(state, main, "b").as_deref(), Some(&b"b\n"[..]));
    assert_eq!(state.db.partition().green_head(hg.root), version);
  });
}

/// A-657 (the namespace, decided). Do: name lines and bookmarks of names stock clients make and of hostile ones.
/// Expect: a stock name reads as itself; `:`, `%`, a control byte and bytes not UTF-8 escaped.
#[test]
fn names_read_as_themselves_and_escape_what_would_meet() {
  assert_eq!(line_name(b"default", 0), "branch:default:0");
  assert_eq!(line_name(b"stable/7.2", 12), "branch:stable/7.2:12");
  assert_eq!(bookmark_name(b"main"), "bookmark:main:");
  assert_eq!(line_name(b"a:b%c\n", 1), "branch:a%3Ab%25c%0A:1");
  assert_eq!(bookmark_name(b"caf\xc3\xa9\xff"), "bookmark:caf\u{e9}%FF:");
  assert_eq!(line_of("branch:a%3Ab:01"), None);
  assert_eq!(
    line_of("branch:%41:0"),
    None,
    "a second spelling of `A` read as a line"
  );
  assert_eq!(line_of("branch:a%3Ab:1"), Some((b"a:b".to_vec(), 1)));
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-657 (the namespace, prefix-free). Do: name lines and bookmarks of arbitrary bytes and numbers. Expect: a line's
  /// name reads back as its branch and number; two names are equal exactly when what they name is; a branch's prefix
  /// begins exactly its own lines' names, a bookmark's name exactly itself; no name of one kind begins as one of the
  /// other; no name is empty or holds a NUL.
  #[test]
  fn names_are_prefix_free_and_read_back(
    first in prop::collection::vec(any::<u8>(), 0..12),
    second in prop::collection::vec(any::<u8>(), 0..12),
    numbers in any::<(u32, u32)>(),
  ) {
    let (one, two) = numbers;
    let line = line_name(&first, one);
    prop_assert_eq!(line_of(&line), Some((first.clone(), one)));
    prop_assert!(!line.is_empty() && !line.contains('\0'));
    prop_assert_eq!(line == line_name(&second, two), first == second && one == two);
    prop_assert_eq!(line_name(&second, two).starts_with(&branch_prefix(&first)), first == second);
    let mark = bookmark_name(&first);
    prop_assert!(!mark.contains('\0') && mark.starts_with(BOOKMARK_TAG) && !mark.starts_with(LINE_TAG));
    prop_assert_eq!(bookmark_name(&second).starts_with(&mark), first == second);
    prop_assert!(line_of(&mark).is_none());
  }
}

/// The stranger, another principal.
fn stranger() -> slates_ipc::protocol::Principal {
  slates_ipc::protocol::Principal::Uid { host: 0, uid: 4321 }
}

/// Sets repository `name`'s policy to `rules`, from version `read`.
fn set_policy(
  state: &mut ShardState,
  (name, read): (&str, u64),
  rules: Vec<slates_ipc::protocol::PolicyRule>,
) {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::SetRepositoryPolicy {
      repository: name.to_owned(),
      read,
      rules,
      hooks: Vec::new(),
      hooks_env: Vec::new(),
      signers: Vec::new(),
    },
  );
  assert_eq!(reply, ReplyBody::Acknowledged);
}

/// A rule over `names` matching `pattern`, deciding a submit with `effect` for `who`.
fn rule(
  names: slates_ipc::protocol::PolicyNames,
  pattern: &str,
  (effect, who): (
    slates_ipc::protocol::PolicyEffect,
    slates_ipc::protocol::PolicyWho,
  ),
) -> slates_ipc::protocol::PolicyRule {
  slates_ipc::protocol::PolicyRule {
    names,
    pattern: pattern.to_owned(),
    right: slates_ipc::protocol::PolicyRight::Submit,
    effect,
    who,
  }
}

/// A-657 part 4 (A-622 part 2's `admit`, §4.17.3 "Push" step 5; `acl`'s words). Do: with a rule denying every
/// principal a submit to branch `default`, prepare stock `push-plain` and ask the policy; with the rule gone, ask again;
/// on another repository whose one rule lets only a stranger submit to bookmarks, prepare stock `push-bookmark`'s move
/// of `main` and ask. Expect: the first refused in `acl`'s words naming the principal, `default` and the first changeset
/// the push brings on it in `short`'s twelve digits; the second admitted (no rule speaks, and the owner holds what none
/// speaks to); the third refused as not allowed on bookmark `main`, naming every digit of the bookmark's new node, the
/// `new` 7.2 hands `prepushkey` for a bookmark (`bundle2_part_handlers.handlebookmark`) and `acl` names.
#[test]
fn the_policy_refuses_before_the_verdict_in_acls_words() {
  use slates_ipc::protocol::{PolicyEffect, PolicyEntry, PolicyNames, PolicyWho};
  crate::daemon::audit_on_shard(|state| {
    let mut plain = Hg::new(state, "acl-plain");
    rebuild(state, &mut plain, (&[1], 1));
    set_policy(
      state,
      ("acl-plain", 0),
      vec![rule(
        PolicyNames::Branches,
        "default",
        (PolicyEffect::Deny, PolicyWho::Anyone),
      )],
    );
    let view = View::of(
      listing!("push-plain-server.txt"),
      listing!("push-plain-bookmarks.txt"),
    );
    let prepared = plain
      .prepare(state, &stock(fixture!("push-plain.hg")), &view)
      .unwrap();
    let denied = format!(
      "acl: user \"{:?}\" denied on branch \"default\" (changeset \"048741fabe42\")",
      owner()
    );
    assert_eq!(
      super::admitted(state, (&owner(), plain.repository), &prepared),
      Err(Refused::Abort {
        message: denied.into_bytes(),
        hint: None
      })
    );
    set_policy(state, ("acl-plain", 1), Vec::new());
    assert_eq!(
      super::admitted(state, (&owner(), plain.repository), &prepared),
      Ok(())
    );
    let mut marks = Hg::new(state, "acl-mark");
    rebuild(state, &mut marks, (&[0], 0));
    let only = PolicyWho::AnyOf {
      entries: vec![PolicyEntry::Is {
        principal: stranger(),
      }],
    };
    set_policy(
      state,
      ("acl-mark", 0),
      vec![rule(
        PolicyNames::Bookmarks,
        "*",
        (PolicyEffect::Allow, only),
      )],
    );
    let view = View::of(
      listing!("push-bookmark-server.txt"),
      listing!("push-bookmark-bookmarks.txt"),
    );
    let prepared = marks
      .prepare(state, &stock(fixture!("push-bookmark.hg")), &view)
      .unwrap();
    let second = "59d5dc23f8786c06d0fae53e625be1cdd640a783";
    let listed = format!(
      "acl: user \"{:?}\" not allowed on bookmark \"main\" (changeset \"{second}\")",
      owner()
    );
    assert_eq!(
      super::admitted(state, (&owner(), marks.repository), &prepared),
      Err(Refused::Abort {
        message: listed.into_bytes(),
        hint: None
      })
    );
  });
}

/// A-657 part 4 (`acl`'s words for a bookmark deleted: 7.2's `bundle2_part_handlers.handlebookmark` hands `prepushkey`
/// the pushkey's `new`, empty for a deletion, and `acl`'s `_pkhook` names it as the changeset). Do: with a rule denying
/// every principal the deletion of any bookmark, prepare a push deleting `main` and ask the policy. Expect: refused in
/// `acl`'s words naming bookmark `main` and an empty changeset, never the node `main` leaves.
#[test]
fn a_bookmark_deleted_is_refused_naming_the_empty_changeset_acl_names() {
  use slates_ipc::protocol::{PolicyEffect, PolicyNames, PolicyRight, PolicyRule, PolicyWho};
  crate::daemon::audit_on_shard(|state| {
    let mut marks = Hg::new(state, "acl-delete");
    rebuild(state, &mut marks, (&[1], 1));
    set_policy(
      state,
      ("acl-delete", 0),
      vec![PolicyRule {
        names: PolicyNames::Bookmarks,
        pattern: "*".to_owned(),
        right: PolicyRight::Delete,
        effect: PolicyEffect::Deny,
        who: PolicyWho::Anyone,
      }],
    );
    let view = View::of(
      listing!("push-plain-server.txt"),
      listing!("push-plain-bookmarks.txt"),
    );
    let deleting = Unbundle {
      parts: vec![taken(
        0,
        Asks::Bookmarks(vec![BookmarkAt {
          name: b"main".to_vec(),
          node: None,
        }]),
      )],
    };
    let prepared = marks.prepare(state, &deleting, &view).unwrap();
    let denied = format!(
      "acl: user \"{:?}\" denied on bookmark \"main\" (changeset \"\")",
      owner()
    );
    assert_eq!(
      super::admitted(state, (&owner(), marks.repository), &prepared),
      Err(Refused::Abort {
        message: denied.into_bytes(),
        hint: None
      })
    );
  });
}

/// A-657 part 4 (`acl`'s order: 7.2's `_txnhook` walks the pushed changesets in revision order and names the first it
/// refuses). Do: with a rule denying every principal a submit to any branch, prepare a push whose first changeset opens
/// branch `stable` and whose second moves `default`'s line, and ask the policy. Expect: refused naming `stable` and its
/// changeset, the branch `acl` meets first, though the line the push moves is `default`'s.
#[test]
fn the_policy_names_the_branch_acl_meets_first() {
  use slates_ipc::protocol::{PolicyEffect, PolicyNames, PolicyWho};
  crate::daemon::audit_on_shard(|state| {
    let mut two = Hg::new(state, "acl-order");
    let made = rebuild(state, &mut two, (&[1], 1));
    set_policy(
      state,
      ("acl-order", 0),
      vec![rule(
        PolicyNames::Branches,
        "*",
        (PolicyEffect::Deny, PolicyWho::Anyone),
      )],
    );
    let view = View::of(
      listing!("push-plain-server.txt"),
      listing!("push-plain-bookmarks.txt"),
    );
    let pushed = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[4, 3])))],
    };
    let prepared = two.prepare(state, &pushed, &view).unwrap();
    let stable = hex(&made[4].node);
    let denied = format!(
      "acl: user \"{:?}\" denied on branch \"stable\" (changeset \"{}\")",
      owner(),
      stable.get(..12).unwrap()
    );
    assert_eq!(
      super::admitted(state, (&owner(), two.repository), &prepared),
      Err(Refused::Abort {
        message: denied.into_bytes(),
        hint: None
      })
    );
  });
}

/// A-657 part 4's hooks (§4.17.3 "Hooks"; 7.2's `hook.py`), run through the hook driver with this test binary playing
/// each hook (`hook_driver::tests::a_hook_role`).
#[cfg(unix)]
mod hooks {
  use std::sync::mpsc::channel;

  use slates_db::catalog::{HookConfig, HookPointName, RepositoryPolicy};
  use slates_hg::push::reply::{Refused, Replied};
  use slates_rt::runtime::{LocalRuntime, RuntimeConfig};

  use super::super::{Hook, HooksRan, admission_hooks, observing_hooks, run_hooks, transaction_id};
  use super::NAME;
  use crate::vcs_hooks::{CommittedView, EnvValue, PendingView};

  /// Format: the variable that makes the test binary, re-invoked, a hook of the role it names (hook_driver's tests').
  const ROLE: &str = "SLATES_HOOK_ROLE";
  /// Format: the libtest name of the role test the binary is re-invoked with.
  const ROLE_TEST: &str = "hook_driver::tests::a_hook_role";
  /// Shape: how many bytes of a hook's words the test keeps.
  const CAP: usize = 1 << 16;
  /// Shape: how long a test lets a push's hooks take before giving up on them: ten liveness windows (`hook_driver`'s
  /// and `svn_hooks`' tests), so a hook that never ends fails its test rather than hanging the suite.
  const TEST_BOUND_NS: u64 = 10 * crate::daemon::LIVENESS_BUDGET_NS;
  /// Shape: the changegroup part's id the hooks' output names.
  const PART: u32 = 4;

  /// A one-shard runtime for a driven run, as hook_driver's tests make one.
  fn runtime() -> LocalRuntime {
    LocalRuntime::new(&RuntimeConfig {
      shards: 1,
      tasks_per_shard: 8,
      timers_per_shard: 8,
      ring_entries: 8,
      step_budget_ns: 1_000_000,
      timer_tick_ns: 100_000,
      batch: 8,
      pin: false,
      cores: Vec::new(),
      page_bytes: 4096,
      spin_ns: 0,
      wake_tracking: None,
    })
    .expect("a local runtime")
  }

  /// A policy configuring `hooks`, each a point and a name.
  fn policy(hooks: &[(HookPointName, &str)]) -> RepositoryPolicy {
    RepositoryPolicy {
      version: 1,
      signers: Vec::new(),
      rules: Vec::new(),
      hooks: hooks
        .iter()
        .map(|(point, name)| HookConfig {
          point: *point,
          name: (*name).to_owned(),
          program: "/usr/local/bin/hook".to_owned(),
        })
        .collect(),
      hooks_env: Vec::new(),
    }
  }

  /// `hook` played by this test binary in `role`.
  fn playing(mut hook: Hook, role: &str) -> Hook {
    hook.program = std::env::current_exe().unwrap();
    hook.input.args = [ROLE_TEST, "--exact", "--nocapture", "--"]
      .iter()
      .map(|arg| arg.as_bytes().to_vec())
      .collect();
    hook
      .input
      .env
      .push((ROLE.to_owned(), EnvValue::Bytes(role.as_bytes().to_vec())));
    hook
  }

  /// Runs `hooks` to their answer, the test binary's directory standing for the pending view; the test gives up past
  /// [`TEST_BOUND_NS`]. The bound's timer is also what keeps the runtime parked for the hooks' events: with no timer
  /// armed, `run_until_idle` returns at its first poll that finds nothing ready, and the run was left unanswered.
  fn drive(hooks: Vec<Hook>) -> HooksRan {
    let runtime = runtime();
    let (sent, got) = channel();
    let view = std::env::current_exe()
      .unwrap()
      .parent()
      .unwrap()
      .to_path_buf();
    runtime
      .spawn(async move {
        let mut gone = core::future::pending::<()>();
        let running = run_hooks((&hooks, &[]), (&view, CAP, Some(PART)), &mut gone);
        let answered = slates_rt::futures::within(TEST_BOUND_NS, running).await;
        let _ = sent.send(answered.ok().flatten());
      })
      .expect("the run's task");
    runtime.run_until_idle();
    got
      .recv()
      .ok()
      .flatten()
      .expect("the hooks answered within the test's bound")
  }

  /// The value a hook's environment gives `name`.
  fn env(hook: &Hook, name: &str) -> Option<EnvValue> {
    hook
      .input
      .env
      .iter()
      .find(|(held, _)| held == name)
      .map(|(_, value)| value.clone())
  }

  /// A-657 part 4 (§4.17.3 "Hooks": admission; 7.2's `apply` and `_exthook`). Do: configure three
  /// `pretxnchangegroup` hooks around a `changegroup` one, take the admission hooks of a push adding changesets
  /// 0101… to 0202… and of one adding none, then run them, the first accepting, the second refusing with status 3, the
  /// third accepting. Expect: the three admission hooks in their order with `HG_NODE` and `HG_NODE_LAST` the first and
  /// last changesets, `HG_PENDING` the view and `HG_URL` the transport's; none for a push adding nothing; the one
  /// observing hook; every hook of the push in its one transaction (`HG_TXNID`); the run stops at the second,
  /// answering `pretxnchangegroup.second hook exited with status 3`, both hooks' words salvaged as `output` parts
  /// naming the changegroup part, two lessons learned, the third never run.
  #[test]
  fn admission_hooks_run_in_order_and_the_first_refusal_answers_in_7_2s_words() {
    let policy = policy(&[
      (
        HookPointName::HgPretxnchangegroup,
        "pretxnchangegroup.first",
      ),
      (HookPointName::HgChangegroup, "changegroup.after"),
      (
        HookPointName::HgPretxnchangegroup,
        "pretxnchangegroup.second",
      ),
      (
        HookPointName::HgPretxnchangegroup,
        "pretxnchangegroup.third",
      ),
    ]);
    let added = Some(([1; 20], [2; 20]));
    let transaction = transaction_id(&NAME);
    let url = "remote:ssh:127.0.0.1";
    let hooks = admission_hooks(&policy, (added, &transaction), (PendingView(7), url));
    assert_inputs(&hooks, url);
    assert!(admission_hooks(&policy, (None, &transaction), (PendingView(7), url)).is_empty());
    let observing = observing_hooks(&policy, (added, &transaction), (CommittedView(7), url));
    assert_eq!(observing.len(), 1);
    let in_transaction = Some(EnvValue::Bytes(transaction.as_bytes().to_vec()));
    assert!(
      hooks
        .iter()
        .chain(&observing)
        .all(|hook| env(hook, "HG_TXNID") == in_transaction)
    );
    let roles = ["accept", "refuse", "accept"];
    let ran = drive(
      hooks
        .into_iter()
        .zip(roles)
        .map(|(hook, role)| playing(hook, role))
        .collect(),
    );
    assert_ran(&ran);
  }

  /// The three admission hooks in order, the first's changesets, view and url as 7.2 names them.
  fn assert_inputs(hooks: &[Hook], url: &str) {
    let names: Vec<&str> = hooks.iter().map(|hook| hook.name.as_str()).collect();
    assert_eq!(
      names,
      [
        "pretxnchangegroup.first",
        "pretxnchangegroup.second",
        "pretxnchangegroup.third"
      ]
    );
    let first = hooks.first().unwrap();
    assert_eq!(
      env(first, "HG_NODE"),
      Some(EnvValue::Bytes(b"01".repeat(20)))
    );
    assert_eq!(
      env(first, "HG_NODE_LAST"),
      Some(EnvValue::Bytes(b"02".repeat(20)))
    );
    assert_eq!(env(first, "HG_PENDING"), Some(EnvValue::ViewRoot));
    assert_eq!(
      env(first, "HG_URL"),
      Some(EnvValue::Bytes(url.as_bytes().to_vec()))
    );
  }

  /// The run stopped at the second hook, in 7.2's words, both hooks' words salvaged naming the changegroup part.
  fn assert_ran(ran: &HooksRan) {
    let message = b"pretxnchangegroup.second hook exited with status 3".to_vec();
    assert_eq!(
      ran.refused,
      Some(Refused::Abort {
        message,
        hint: None
      })
    );
    assert_eq!(ran.lessons.len(), 2);
    let named: Vec<Option<u32>> = ran
      .output
      .iter()
      .map(|part| match part {
        Replied::Output { in_reply_to, words } if !words.is_empty() => *in_reply_to,
        _ => None,
      })
      .collect();
    assert_eq!(named, [Some(PART), Some(PART)]);
  }
}
