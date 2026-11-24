//! A-657's server tests: Mercurial pushes landed on a daemon's owning shard through A-635's append, held to what stock
//! 7.2 answered (`crates/hg/fixtures/push`, made by `make-push-fixtures.sh`): the fixtures' repository rebuilt from its
//! texts in a new Mercurial repository (every node 7.2's), then each stock push landed on it and its reply compared
//! byte for byte with 7.2's.

use std::collections::BTreeSet;

use proptest::prelude::*;
use slates_db::catalog::{Principal, RepositoryId, StateLog, VolumeId as DbVolumeId};
use slates_hg::bundle2::Bundle2;
use slates_hg::changegroup::{Changegroup, Group, Revision, Version};
use slates_hg::index::Log;
use slates_hg::phases::{Phase, PhaseHead};
use slates_hg::push::check::HeadsArgument;
use slates_hg::push::reply::{Refused, Replied, Reply};
use slates_hg::push::{Asks, BookmarkAt, ChangegroupPart, Outcome, Taken, Unbundle, read};
use slates_hg::serve::{Repository, Rev};
use slates_hg::{Admission, NULL_NODE, Node, delta, node};
use slates_ipc::protocol::{ObjectFormat, ReplyBody, RequestBody, VcsTool};
use slates_merge::push::{HEAD_ATTRIBUTE, ROOT};

use super::{
  BOOKMARK_TAG, LINE_TAG, Landed, Prepared, bookmark_name, bookmark_of, branch_prefix, line_name,
  line_of, transaction_id,
};
use crate::state::ShardState;
use crate::verbs::{dispatch, to_db_volume};

/// Shape: the room a test's work is built within, far past any of these trees.
const ROOM: u64 = 1 << 20;
/// Shape: how long a test waits for a shard's task (a rebuild) to end before it fails, as the import tests wait.
const TASK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
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

/// One changeset of a test's served view: its node, its parents' revisions, its branch and its phase.
type ViewChangeset = (Node, [Option<Rev>; 2], Vec<u8>, Phase);

/// A served view a test builds from a fixture's listings: `{rev} {node} {p1} {p2} {phase} {branch}` a line, and
/// `{name} {node}` a bookmark.
#[derive(Clone, Debug, Default)]
struct View {
  changesets: Vec<ViewChangeset>,
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
  fn markers(&self) -> impl Iterator<Item = slates_hg::obsmarkers::Marker<'_>> {
    core::iter::empty()
  }

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

  /// The view hides nothing, so the roots its register would keep are its draft changesets none of whose parents is
  /// draft.
  fn draft_roots(&self) -> impl Iterator<Item = Node> {
    self
      .changesets
      .iter()
      .filter(|held| {
        held.3 == Phase::Draft
          && held
            .1
            .iter()
            .flatten()
            .all(|parent| self.phase(*parent) == Phase::Public)
      })
      .map(|held| held.0)
  }

  fn tags_file(
    &self,
    _rev: slates_hg::serve::Rev,
    _admission: &mut slates_hg::Admission,
  ) -> Result<Option<Node>, slates_hg::serve::Refusal> {
    Ok(None)
  }
}

/// A changeset of the fixtures' repository as `make-push-fixtures.sh` made it with stock 7.2 (user `test`, date `0
/// 0`): its description, the one file it adds and that file's bytes, its parent among those made before it, and its
/// branch.
pub(crate) type Commit = (
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
pub(crate) struct Made {
  pub(crate) node: Node,
  pub(crate) p1: Node,
  pub(crate) text: Vec<u8>,
  pub(crate) manifest: (Node, Node, Vec<u8>),
  pub(crate) file: (Vec<u8>, Node, Vec<u8>),
}

/// The fixtures' repository rebuilt from its texts as 7.2's `commit` writes them (§4.17.3 decision 7's fields).
fn history() -> Vec<Made> {
  history_of(&COMMITS)
}

/// `commits` rebuilt from their texts as 7.2's `commit` writes them, each adding its one file to its parent's.
pub(crate) fn history_of(commits: &[Commit]) -> Vec<Made> {
  let mut made: Vec<Made> = Vec::new();
  let mut manifests: Vec<Vec<(Vec<u8>, Node)>> = Vec::new();
  for &(description, path, bytes, parent, branch) in commits {
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
pub(crate) fn changegroup(made: &[Made], which: &[usize]) -> ChangegroupPart {
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

/// A Mercurial repository on the shard, whose pushes keep their revisions in its store there (A-662's Mercurial arm).
struct Hg {
  repository: RepositoryId,
  root: DbVolumeId,
}

impl Hg {
  /// A new Mercurial repository `name`, its first line `default`'s, empty.
  fn new(state: &mut ShardState, name: &str) -> Hg {
    let reply = crate::test_store::create(
      state,
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
    }
  }

  fn prepare(
    &self,
    state: &mut ShardState,
    unbundle: &Unbundle,
    view: &View,
  ) -> Result<Prepared, Box<Refused>> {
    super::prepare_whole(
      state,
      self.repository,
      unbundle,
      view,
      (false, NAME, ROOM),
      &mut room(),
    )
  }

  fn land(&mut self, state: &mut ShardState, prepared: Prepared) -> Landed {
    let landed = super::land_stored(state, (&owner(), self.repository), prepared);
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
  ) -> Result<Reply, Box<Refused>> {
    let prepared = self.prepare(state, unbundle, view)?;
    self.land(state, prepared).answer.map_err(Box::new)
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

  /// How many changesets the repository's index in its store holds.
  fn changesets(&self, state: &ShardState) -> u32 {
    let (content, store) = state.vcs_stores.mercurial_ref(self.repository).unwrap();
    store.index.len(content, Log::Changelog)
  }

  /// Whether the repository's index in its store holds changeset `node`.
  fn holds(&self, state: &ShardState, node: &Node) -> bool {
    let (content, store) = state.vcs_stores.mercurial_ref(self.repository).unwrap();
    store.index.rev(content, Log::Changelog, node).is_some()
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
  crate::vcs_store::named_objects::read_file(state, green, path)
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
  assert!(hg.holds(state, &made[0].node) && hg.holds(state, &made[1].node));
}

/// A-887 (main 01a20182's order of 2026-11-23 on the greens' 56 MB: the 57k push's `Green::make_mapped` bytes, live at
/// its end). Do: rebuild the fixtures' repository as one push to its empty first line, then look at every file of its
/// root green's head. Expect: each file whose bytes the repository's content store holds whole (by their identity) held
/// by that name (`Content::Named`), none of them as bytes, and some such files (non-vacuity). Failing first: every one
/// held as bytes, the green's second copy of what the store holds.
#[test]
fn a_pushed_file_the_content_store_holds_whole_is_held_by_its_name() {
  crate::test_store::audit_on_stored_shard("hg", |state| {
    let mut hg = Hg::new(state, "named");
    let _ = rebuild(state, &mut hg, (&[1], 1));
    let content = state.vcs_stores.content().unwrap();
    let engine = state.greens.get(&hg.root).unwrap();
    let (mut named, mut as_bytes) = (0usize, Vec::new());
    for (path, held) in engine.files() {
      match held {
        slates_merge::engine::Content::Named(_) => named += 1,
        slates_merge::engine::Content::Bytes(bytes) => {
          if content.contains(&content.object_id(bytes)) {
            as_bytes.push(path);
          }
        }
      }
    }
    assert!(
      named + as_bytes.len() > 0,
      "the push's files are in the content store (non-vacuity)"
    );
    assert!(
      as_bytes.is_empty(),
      "held as bytes though the content store holds them whole: {as_bytes:?} ({named} named)"
    );
  });
}

/// A-887 (main 01a20182's ruling (a), its red for a mount's read). Do: on a store-backed daemon, rebuild the fixtures'
/// repository as one push to its empty first line; check its root green holds file `b` by its object's name, that
/// object let go from the content store, as a push's texts are once kept; then read `b` through its name on the shard,
/// as a mount's read does (`named_objects::read_file_brought`). Expect: `b`'s bytes, `b\n`, its windows brought back by
/// the revision the green's changeset names for `b` (`hg_named`, main's ruling (B) of 2026-11-24). Failing first: none,
/// the read-through finding no object of that identity on the node's store (a Mercurial file revision's bytes lie there
/// as a delta, by its text's name).
#[test]
fn a_named_pushed_file_let_go_is_read_back_through_its_name() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (green, named_away) = crate::daemon::observe_first(&daemon, |state| {
    let mut hg = Hg::new(state, "named-read");
    let _ = rebuild(state, &mut hg, (&[1], 1));
    let content = state.vcs_stores.content().unwrap();
    let held = state
      .greens
      .get(&hg.root)
      .unwrap()
      .content("b")
      .map(|held| match held {
        slates_merge::engine::Content::Named(identity) => {
          content.is_away(&slates_vcs::ContentId(identity.hash))
        }
        slates_merge::engine::Content::Bytes(_) => false,
      });
    (hg.root, held)
  });
  let served = crate::test_store::run_on_first(
    &daemon,
    move || async move { crate::vcs_store::named_objects::read_file_brought(green, "b").await },
    std::time::Duration::from_secs(60),
  );
  daemon.stop();
  assert_eq!(
    named_away,
    Some(true),
    "`b` held by its name, its object let go"
  );
  assert_eq!(
    served.as_deref(),
    Some(&b"b\n"[..]),
    "read back through its name"
  );
}

/// The reply recorded for request `id` of `origin`, waited for a step at a time within [`NAMED_PATIENCE`]. A parked
/// request whose bring-back was refused is answered unrecorded (`verbs::park_on_away`), so the wait fails at once on
/// one (`verbs.bring_back_refused`).
async fn recorded_answer(
  origin: u64,
  id: slates_wire::request::RequestId,
) -> ReplyBody {
  use crate::repository::svn_commit::tests::on;
  let deadline = slates_rt::futures::now_ns()
    .saturating_add(u64::try_from(NAMED_PATIENCE.as_nanos()).unwrap_or(u64::MAX));
  loop {
    let seen = on(|state| {
      state
        .db
        .partition()
        .completion(origin, id.client, id.sequence)
    });
    if let slates_wire::request::Seen::Completed(bytes) = seen {
      return <ReplyBody as slates_wire::Wire>::from_bytes(&bytes).expect("a reply");
    }
    let refused = on(|state| {
      state
        .refusals
        .get("verbs.bring_back_refused")
        .copied()
        .unwrap_or(0)
    });
    assert_eq!(refused, 0, "request {id:?}'s bring-back was refused");
    assert!(
      slates_rt::futures::now_ns() < deadline,
      "request {id:?} never answered"
    );
    slates_rt::futures::yield_now().await;
  }
}

/// Shape: how long a named-file test's history may run on its shard.
const NAMED_PATIENCE: std::time::Duration = std::time::Duration::from_secs(120);

/// Over a work on green `green`, `x` inserted at offset 1 of `b`, then the work submitted as a client's recorded request
/// (A-887's in-place edit of a named file): the submit's answer, and how many requests parked on a let-go window meanwhile
/// (`verbs.parked_on_away`).
async fn edited_named(green: DbVolumeId) -> (ReplyBody, u64) {
  use crate::repository::svn_commit::tests::on;
  let parked = || {
    on(|state| {
      state
        .refusals
        .get("verbs.parked_on_away")
        .copied()
        .unwrap_or(0)
    })
  };
  let made = on(|state| {
    crate::vcs_store::paths::tests::ask(
      state,
      RequestBody::CreateWork {
        green: slates_ipc::protocol::VolumeId { bytes: green.bytes },
        name: "edit-named".to_owned(),
      },
    )
  });
  let ReplyBody::WorkCreated { id: work, .. } = made else {
    panic!("no work: {made:?}")
  };
  let edited = on(|state| {
    crate::vcs_store::paths::tests::ask(
      state,
      RequestBody::Edit {
        work,
        path: "b".to_owned(),
        at: 1,
        delete_len: 0,
        bytes: b"x".to_vec(),
      },
    )
  });
  assert!(!matches!(edited, ReplyBody::Refused { .. }), "{edited:?}");
  let before = parked();
  let submit = slates_wire::request::RequestId {
    client: 7,
    sequence: 1,
  };
  let _ = on(|state| {
    crate::verbs::run_recorded_for_test(
      state,
      (7, submit),
      &owner(),
      crate::identities::authored(work, "edit a named file"),
    )
  });
  let submitted = recorded_answer(7, submit).await;
  (submitted, parked().saturating_sub(before))
}

/// A-887 (main 01a20182's ruling (B) of 2026-11-24 00:16: a restart's recovery). Do: on a daemon over a segment and a
/// store, rebuild the fixtures' repository as one push, its root green holding `b` by its object's name and that object
/// let go, then edit `b` in place over a work and submit it (`edited_named`); stop the daemon and the store's owner,
/// start both again over the same segment and store, and read `b` through its name. Expect: the submit landed before the
/// restart; after it, the green recovered at the submit's version and `b` read `bx\n`, its let-go windows brought back
/// by the revision the recovered green's changeset names. Failing first: the green fenced, or `b` unread.
#[test]
fn a_restart_recovers_a_green_whose_named_pushed_file_a_work_edited() {
  use crate::repository::store_tests::{anchor_of, daemon_over};
  let instance = format!("vcs-store-hg-named-{}", std::process::id());
  let mut store = crate::test_store::TestStore::start("hg-named-restart");
  let anchor = anchor_of(&instance);
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  daemon.bootstrap(true).unwrap();
  let green = crate::daemon::observe_first(&daemon, |state| {
    let mut hg = Hg::new(state, "named-restart");
    let _ = rebuild(state, &mut hg, (&[1], 1));
    hg.root
  });
  let (submitted, _) = crate::test_store::run_on_first(
    &daemon,
    move || async move { edited_named(green).await },
    NAMED_PATIENCE,
  );
  assert!(
    !matches!(submitted, ReplyBody::Refused { .. }),
    "{submitted:?}"
  );
  let version = crate::daemon::observe_first(&daemon, move |state| {
    state.db.partition().green_head(green)
  });
  daemon.stop();
  store.restart_owner();
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let recovered = crate::daemon::observe_first(&daemon, move |state| {
    let head = state.greens.get(&green).map(|engine| engine.head());
    (head, state.fenced_greens.get(&green).cloned())
  });
  let file = crate::test_store::run_on_first(
    &daemon,
    move || async move { crate::vcs_store::named_objects::read_file_brought(green, "b").await },
    NAMED_PATIENCE,
  );
  daemon.stop();
  assert_eq!(
    recovered,
    (Some(version), None),
    "the green recovered at the submit's version, unfenced"
  );
  assert_eq!(file.as_deref(), Some(&b"bx\n"[..]), "the edited file after the restart");
}

/// A-887 (main 01a20182's ruling (B) of 2026-11-24 00:16: an in-place edit after naming). Do: on a store-backed daemon,
/// rebuild the fixtures' repository as one push, its root green holding `b` by its object's name and that object let
/// go; over a work on the root green, insert `x` at offset 1 of `b`, then submit the work as a client's recorded
/// request. Expect: the submit answered, not refused; the verb parked on the let-go window and resumed
/// (`verbs.parked_on_away` moved); the green's `b` then `bx\n`, read through its name. Failing first: the parked
/// submit's bring-back refused `Missing { at: 0 }` and the submit never answered recorded: its judge noted the read with
/// no green or path, so the read-through looked for the object by identity on the node's store, which keeps a
/// Mercurial revision by its path and node.
#[test]
fn a_work_editing_a_named_pushed_file_in_place_brings_its_text_back_and_lands() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (green, named_away) = crate::daemon::observe_first(&daemon, |state| {
    let mut hg = Hg::new(state, "named-edit");
    let _ = rebuild(state, &mut hg, (&[1], 1));
    let content = state.vcs_stores.content().unwrap();
    let held = state
      .greens
      .get(&hg.root)
      .unwrap()
      .content("b")
      .map(|held| match held {
        slates_merge::engine::Content::Named(identity) => {
          content.is_away(&slates_vcs::ContentId(identity.hash))
        }
        slates_merge::engine::Content::Bytes(_) => false,
      });
    (hg.root, held)
  });
  let (submitted, parked, file) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let (submitted, parked) = edited_named(green).await;
      let file = crate::vcs_store::named_objects::read_file_brought(green, "b").await;
      (submitted, parked, file)
    },
    NAMED_PATIENCE,
  );
  daemon.stop();
  assert_eq!(
    named_away,
    Some(true),
    "`b` held by its name, its object let go"
  );
  assert!(
    !matches!(submitted, ReplyBody::Refused { .. }),
    "{submitted:?}"
  );
  assert!(parked > 0, "the submit never parked on the let-go window");
  assert_eq!(file.as_deref(), Some(&b"bx\n"[..]), "the edit landed");
}

/// A-887 (main 01a20182's ruling (B) of 2026-11-24 00:16: the window before a push's entry commits, a second work naming
/// the same bytes). Do: on a store-backed daemon, push the fixtures' `base` and `second` to an empty repository with
/// bookmarks `main`, `left` and `right` all at `second`, so one work is judged for three greens (two judge a clone,
/// which carries none of the run's bytes); its works are built, then its segment lands and lets its texts go, then it
/// is judged, as the tests' driver orders it. Expect: the push landed; each of the three bookmarks' greens holds `b` by
/// its name and reads it `b\n` through it. Failing first: the push refused `ContentUnavailable`, a clone's named run
/// unread at its judge, its text let go before the entry committed.
#[test]
fn a_second_work_naming_a_pushed_text_let_go_before_its_entry_reads_it_back() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let greens = crate::daemon::observe_first(&daemon, |state| {
    let mut hg = Hg::new(state, "named-twice");
    let made = history();
    let at = |name: &[u8]| BookmarkAt {
      name: name.to_vec(),
      node: Some(made[1].node),
    };
    let unbundle = Unbundle {
      parts: vec![
        taken(0, Asks::Changegroup(changegroup(&made, &[0, 1]))),
        taken(1, Asks::Bookmarks(vec![at(b"main"), at(b"left"), at(b"right")])),
      ],
    };
    let answer = hg.push(state, &unbundle, &View::default());
    assert!(answer.is_ok(), "the push was refused: {answer:?}");
    let greens = [b"main".as_slice(), b"left", b"right"]
      .map(|name| hg.bound(state, &bookmark_name(name)).expect("a bookmark's green"));
    let named = greens.map(|green| {
      matches!(
        state.greens.get(&green).and_then(|engine| engine.content("b").map(|held| {
          matches!(held, slates_merge::engine::Content::Named(_))
        })),
        Some(true)
      )
    });
    assert_eq!(named, [true; 3], "`b` held by its name in each green");
    greens
  });
  let read = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let mut read = Vec::new();
      for green in greens {
        read.push(crate::vcs_store::named_objects::read_file_brought(green, "b").await);
      }
      read
    },
    NAMED_PATIENCE,
  );
  daemon.stop();
  for (green, file) in greens.iter().zip(&read) {
    assert_eq!(file.as_deref(), Some(&b"b\n"[..]), "{green:?}'s `b`");
  }
}

/// A-657, AC-HG.3 (T-HG.3; golden, by meaning and byte for byte). Do: rebuild the fixtures' repository from its texts
/// in a new Mercurial repository (`base` and `second` landed as one push to its empty first line, both published by a
/// `phase-heads` part, bookmark `main` made at `second`), then land stock 7.2's `push-plain`, then land it again
/// without its race checks (a client that heard no answer). Expect: the rebuilt nodes 7.2's; `main` a green of its
/// own at `second`, no draft root; `push-plain` answered byte for byte as 7.2 answered it, `default`'s line at its
/// changeset, which is a draft root and held by the index; sent again it is answered alike and records nothing.
#[test]
fn a_stock_push_lands_on_the_rebuilt_repository_as_7_2_answered_it() {
  crate::test_store::audit_on_stored_shard("hg", |state| {
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

/// A-680 part 2c, A-767 C4. Do: on a running daemon holding a changeset store, land stock `push-plain` on the rebuilt
/// repository, then make the repository's store again from its log as a recovery does (`vcs_store::rebuild`, the
/// sliced task), and read the store once the task ends. Expect the store's index to hold every changeset the pushes
/// brought, the stock push's among them, as many as before: each push's entry names its changeset on the store, whose
/// payload the replay read back, in order; and its stream clone still advertised (goal 93 perf, main's ruling of
/// 2026-11-21 02:31), since the store keeps every revision it replayed.
#[test]
fn a_landed_push_is_replayed_into_the_store_from_its_entry() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (repository, made, before) = crate::daemon::observe_first(&daemon, |state| {
    let mut hg = Hg::new(state, "replayed");
    let made = rebuild(state, &mut hg, (&[1], 1));
    let view = View::of(
      listing!("push-plain-server.txt"),
      listing!("push-plain-bookmarks.txt"),
    );
    let answer = hg.push(state, &stock(fixture!("push-plain.hg")), &view);
    assert!(answer.is_ok(), "{answer:?}");
    let before = hg.changesets(state);
    crate::vcs_store::rebuild(state, hg.repository);
    (hg.repository, made, before)
  });
  let began = std::time::Instant::now();
  let rebuilding = loop {
    let rebuilding = crate::daemon::observe_first(&daemon, move |state| {
      state.vcs_stores.rebuilding(repository)
    });
    if !rebuilding || began.elapsed() > TASK_WAIT {
      break rebuilding;
    }
  };
  let nodes: Vec<Node> = [0, 1, 3].iter().map(|at| made[*at].node).collect();
  let (after, held, streams) = crate::daemon::observe_first(&daemon, move |state| {
    let (content, store) = state.vcs_stores.mercurial_ref(repository).unwrap();
    let counted = (
      store.index.len(content, Log::Changelog),
      nodes
        .iter()
        .all(|node| store.index.rev(content, Log::Changelog, node).is_some()),
    );
    let streams =
      crate::vcs_store::hg_view::with_view(state, repository, &mut room(), |view, _, _| {
        slates_hg::serve::getbundle::Store::streams(view)
      });
    (counted.0, counted.1, streams)
  });
  daemon.stop();
  assert!(!rebuilding, "still rebuilding after {TASK_WAIT:?}");
  assert_eq!(after, before);
  assert!(held);
  assert_eq!(
    streams,
    Some(true),
    "a push replayed from the changeset log keeps its revisions in the store, so its stream clone stays advertised"
  );
}

/// Shape: each large file's length in [`LARGE`]: two of the store's 64 KiB pieces, so the payload spans many pieces
/// while any one delta spans few.
const LARGE_LEN: usize = 1 << 17;
/// Six large files, each its own byte repeated.
static LARGE: [[u8; LARGE_LEN]; 6] = [
  [b'a'; LARGE_LEN],
  [b'b'; LARGE_LEN],
  [b'c'; LARGE_LEN],
  [b'd'; LARGE_LEN],
  [b'e'; LARGE_LEN],
  [b'f'; LARGE_LEN],
];

/// Format: the segment bound a test's push is cut at: a 128 KiB file's revision a segment, about.
const SEGMENT_BYTES: u64 = 128 * 1024;

/// A-767 C1 (R2, a replay holding no payload whole), since main 01a20182's ruling (b1) of 2026-11-22 04:44 a push's run
/// of segment changesets. Do: on a running daemon holding a changeset store, push six changesets each adding a 128 KiB
/// file (a payload of about 768 KiB, cut at [`SEGMENT_BYTES`]); make the repository's store again from its log (the sliced
/// `vcs_store::rebuild`) and read it once the task ends. Expect every changeset held again; the push landed as more than
/// one segment and its run replayed segment by segment; no byte of its payload read back, its rows appended by
/// identity; and each segment's record written over paced turns, at least one a segment (goal 93, r499: a whole record
/// in one shard step ran 316 ms; red before, no record turn counted). Before (b1) the push's one changeset replayed
/// from its payload a piece at a time, a want a piece.
#[test]
fn a_landed_push_replays_as_its_run_of_segments_reading_no_payload() {
  let commits: Vec<Commit> = LARGE
    .iter()
    .zip(["one", "two", "three", "four", "five", "six"])
    .enumerate()
    .map(|(at, (bytes, name))| (name, name, bytes.as_slice(), at.checked_sub(1), "default"))
    .collect();
  let made = history_of(&commits);
  let part = changegroup(&made, &[0, 1, 2, 3, 4, 5]);
  let payload = u64::try_from(part.payload.len()).unwrap();
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let repository = crate::daemon::observe_first(&daemon, move |state| {
    super::super::hg_run::TEST_SEGMENT_BYTES.with(|bound| bound.set(SEGMENT_BYTES));
    let mut hg = Hg::new(state, "pieces");
    let unbundle = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(part))],
    };
    let answer = hg.push(state, &unbundle, &View::default());
    assert!(answer.is_ok(), "{answer:?}");
    crate::vcs_store::rebuild(state, hg.repository);
    hg.repository
  });
  let began = std::time::Instant::now();
  let rebuilding = loop {
    let rebuilding = crate::daemon::observe_first(&daemon, move |state| {
      state.vcs_stores.rebuilding(repository)
    });
    if !rebuilding || began.elapsed() > TASK_WAIT {
      break rebuilding;
    }
  };
  let counted = |state: &ShardState, name: &str| state.refusals.get(name).copied().unwrap_or(0);
  let (changesets, landed, replayed, wants, record_turns) =
    crate::daemon::observe_first(&daemon, move |state| {
      let (content, store) = state.vcs_stores.mercurial_ref(repository).unwrap();
      (
        store.index.len(content, Log::Changelog),
        counted(state, super::super::hg_run::SEGMENTS_LANDED),
        counted(state, crate::vcs_store::HG_REPLAY_SEGMENTS),
        counted(state, crate::vcs_store::HG_REPLAY_WANTS),
        counted(state, super::SEGMENT_RECORD_TURNS),
      )
    });
  daemon.stop();
  println!(
    "(b1) replay: a {payload}-byte payload landed as {landed} segments, {replayed} replayed, {wants} payload fills"
  );
  assert!(!rebuilding, "still rebuilding after {TASK_WAIT:?}");
  assert_eq!(changesets, 6);
  assert!(landed > 1, "the push landed as {landed} segments");
  assert_eq!(replayed, landed, "its run replayed segment by segment");
  assert_eq!(wants, 0, "no byte of its payload read back");
  // Goal 93 (r499: a segment's whole record in one shard step ran 316 ms): every record written a paced turn at a time.
  assert!(
    record_turns >= landed,
    "{record_turns} record turns for {landed} segments: a record written in one call"
  );
}

fn assert_plain_landed(state: &ShardState, hg: &Hg, made: &[Made]) {
  assert_eq!(
    hex(&made[3].node),
    "048741fabe422ed31728cd666c1e36b6c3f0a96d"
  );
  assert_eq!(head(state, hg.root), Some(made[3].node));
  assert_eq!(file(state, hg.root, "d").as_deref(), Some(&b"d\n"[..]));
  assert_eq!(hg.roots(state), BTreeSet::from([made[3].node]));
  assert!(hg.holds(state, &made[3].node));
}

/// A-657 part 4 (`HG_TXNID`; 7.2's `localrepo.transaction`). Do: prepare stock `push-plain` on the rebuilt repository,
/// and take the transaction ids of the push's name and of another. Expect: the prepared push's id is its name's, `TXN:`
/// and forty lower-case hexadecimal digits; the same name gives the same id, another name another.
#[test]
fn a_push_names_its_transaction_as_7_2_does() {
  crate::test_store::audit_on_stored_shard("hg", |state| {
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
  crate::test_store::audit_on_stored_shard("hg", |state| {
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
  crate::test_store::audit_on_stored_shard("hg", |state| {
    let mut hg = Hg::new(state, "race");
    let made = rebuild(state, &mut hg, (&[1], 1));
    let view = View::of(
      listing!("push-plain-server.txt"),
      listing!("push-plain-bookmarks.txt"),
    );
    // The stock push's changegroup is taken into its fork first; then another push lands; then the stock push lands
    // as the driver lands it, its stale fork re-based and the race judged at landing (A-720 part 2b).
    let bundle = fixture!("push-plain.hg");
    let unbundle = stock(bundle);
    let (_, part) = unbundle.changegroup().unwrap();
    let forked = super::take_whole(
      state,
      hg.repository,
      (part.version, part.payload.clone()),
      &mut room(),
    )
    .unwrap()
    .unwrap();
    let other = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[2])))],
    };
    assert!(hg.push(state, &other, &view).is_ok());
    assert_eq!(head(state, hg.root), Some(made[2].node));
    let landed = super::land_in(
      state,
      hg.repository,
      (&owner(), bundle),
      &unbundle,
      Some(forked),
      &mut room(),
    );
    let race = match landed {
      Some(Outcome::Refused(reply)) => Err(encoded(&reply)),
      other => Ok(format!("{other:?}")),
    };
    assert_eq!(race, Err(fixture!("push-race-reply.hg").to_vec()));
    assert_eq!(head(state, hg.root), Some(made[2].node));
    assert!(!hg.holds(state, &made[3].node));
    assert_eq!(hg.roots(state), BTreeSet::from([made[2].node]));
  });
}

/// A-720 part 2b (pushes taken side by side). Do: on the rebuilt repository, take two pushes' changegroups into their
/// own forks from the same index (`other` and `plain`, each a child of `second`, neither asking a race check), land the
/// first, then land the second, whose fork the first made stale. Expect both accepted and both changesets held by the
/// repository's index after: the second re-based by node, nothing of the first lost.
#[test]
fn two_pushes_taken_side_by_side_both_land() {
  crate::test_store::audit_on_stored_shard("hg", |state| {
    let mut hg = Hg::new(state, "side-by-side");
    let made = rebuild(state, &mut hg, (&[1], 1));
    let first = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[2])))],
    };
    let second = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[3])))],
    };
    let fork_of = |state: &mut ShardState, unbundle: &Unbundle| {
      let (_, part) = unbundle.changegroup().unwrap();
      super::take_whole(
        state,
        hg.repository,
        (part.version, part.payload.clone()),
        &mut room(),
      )
      .unwrap()
      .unwrap()
    };
    let (first_fork, second_fork) = (fork_of(state, &first), fork_of(state, &second));
    let principal = owner();
    for (unbundle, forked) in [(&first, first_fork), (&second, second_fork)] {
      let landed = super::land_in(
        state,
        hg.repository,
        (&principal, b"side"),
        unbundle,
        Some(forked),
        &mut room(),
      );
      assert!(matches!(landed, Some(Outcome::Accepted(_))), "{landed:?}");
    }
    assert!(hg.holds(state, &made[2].node) && hg.holds(state, &made[3].node));
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
/// `default` carrying `check:heads` (the heads it read: `stable`'s changeset alone), its changegroup taken into its fork,
/// then land another push moving `stable`, then land the first as the driver does (A-720: its fork re-based, its checks
/// judged at landing). Expect: `stable`'s line made, numbered 0; the first push's own line never moved, so its verdict
/// alone would accept it, but `stable`'s heads no longer what it read, so it is refused as 7.2's race, and nothing of it
/// is applied.
#[test]
fn a_push_is_refused_when_a_green_it_only_checked_moved() {
  crate::test_store::audit_on_stored_shard("hg", |state| {
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
    // Its changegroup taken into its fork first (A-720); another push moves `stable`; then it lands as the driver lands
    // it, its stale fork re-based and `check:heads` judged at landing.
    let (_, part) = checked.changegroup().unwrap();
    let forked = super::take_whole(
      state,
      hg.repository,
      (part.version, part.payload.clone()),
      &mut room(),
    )
    .unwrap()
    .unwrap();
    let again = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[5])))],
    };
    assert!(hg.push(state, &again, &view_with(&made, &[4])).is_ok());
    assert_eq!(head(state, stable), Some(made[5].node));
    let raced = super::land_in(
      state,
      hg.repository,
      (&owner(), b"checked"),
      &checked,
      Some(forked),
      &mut room(),
    );
    let words = super::RACED.to_vec();
    assert_eq!(
      raced,
      Some(Outcome::refused(&Refused::PushRaced { message: words }))
    );
    assert_eq!(head(state, hg.root), Some(made[1].node));
    assert!(!hg.holds(state, &made[3].node));
  });
}

/// A-657 (§4.17.3 decision 3: phases moved in the same entry). Do: rebuild with only `base` published and `main` at
/// `second`, then land stock `push-phase` (its `phase-heads` part publishes `second`). Expect: before it, `second` the
/// one draft root; it is answered byte for byte as 7.2 answered it (a stream with no part), the root removed, the
/// phases log one position on.
#[test]
fn a_published_draft_leaves_the_roots_in_the_same_entry() {
  crate::test_store::audit_on_stored_shard("hg", |state| {
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
  crate::test_store::audit_on_stored_shard("hg", |state| {
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

/// A-657 (the namespace, decided), A-680 part 2 (a bookmark's name read back). Do: name lines and bookmarks of names
/// stock clients make and of hostile ones, and read bookmarks' names back. Expect: a stock name reads as itself; `:`,
/// `%`, a control byte and bytes not UTF-8 escaped; a bookmark's name read back as its bookmark, and a second spelling
/// of one, a line's name or a name without its end read as no bookmark.
#[test]
fn names_read_as_themselves_and_escape_what_would_meet() {
  assert_names_escape();
  assert_lines_read_back();
  assert_bookmarks_read_back();
}

/// A stock name reads as itself; `:`, `%`, a control byte and bytes not UTF-8 escaped.
fn assert_names_escape() {
  assert_eq!(line_name(b"default", 0), "branch:default:0");
  assert_eq!(line_name(b"stable/7.2", 12), "branch:stable/7.2:12");
  assert_eq!(bookmark_name(b"main"), "bookmark:main:");
  assert_eq!(line_name(b"a:b%c\n", 1), "branch:a%3Ab%25c%0A:1");
  assert_eq!(bookmark_name(b"caf\xc3\xa9\xff"), "bookmark:caf\u{e9}%FF:");
}

/// A line's name reads back as its branch and number, and a second spelling of one as none.
fn assert_lines_read_back() {
  assert_eq!(line_of("branch:a%3Ab:01"), None);
  assert_eq!(
    line_of("branch:%41:0"),
    None,
    "a second spelling of `A` read as a line"
  );
  assert_eq!(line_of("branch:a%3Ab:1"), Some((b"a:b".to_vec(), 1)));
}

/// A bookmark's name reads back as its bookmark; a second spelling of one, a line's name or a name without its end
/// reads as no bookmark.
fn assert_bookmarks_read_back() {
  assert_eq!(bookmark_of("bookmark:main:"), Some(b"main".to_vec()));
  assert_eq!(
    bookmark_of("bookmark:caf\u{e9}%FF:"),
    Some(b"caf\xc3\xa9\xff".to_vec())
  );
  assert_eq!(
    bookmark_of("bookmark:%41:"),
    None,
    "a second spelling of `A`"
  );
  assert_eq!(bookmark_of("bookmark:main"), None, "a name without its end");
  assert_eq!(bookmark_of("branch:main:0"), None, "a line's name");
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
    prop_assert_eq!(bookmark_of(&mark), Some(first.clone()));
    prop_assert!(bookmark_of(&line).is_none());
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
      hg_publishing: true,
    },
  );
  assert_eq!(reply, ReplyBody::Acknowledged);
}

/// Configures repository `name`'s policy, from its first version, to run the `pretxnchangegroup` hook `hook`, its
/// program the test binary, a path absolute on the test's host, as the guard asks (A-622).
fn set_hook(state: &mut ShardState, name: &str, hook: &str) {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::SetRepositoryPolicy {
      repository: name.to_owned(),
      read: 0,
      rules: Vec::new(),
      hooks: vec![slates_ipc::protocol::HookConfig {
        point: slates_ipc::protocol::HookPointName::HgPretxnchangegroup,
        name: hook.to_owned(),
        program: std::env::current_exe()
          .unwrap()
          .to_string_lossy()
          .into_owned(),
      }],
      hooks_env: Vec::new(),
      signers: Vec::new(),
      hg_publishing: true,
    },
  );
  assert_eq!(reply, ReplyBody::Acknowledged);
}

/// A-680 part 2b (`hg_push`'s module docs, "The driver" and "The hook gate"; golden: stock 7.2's `push-plain`). Do:
/// rebuild the fixtures' repository twice, configure a `pretxnchangegroup` hook on the second, then send stock
/// `push-plain`'s bundle through the driver both edges call, `heads` `force` as 7.2's client sends it with a bundle2, to
/// the hooked repository and then to the first. Expect: the hooked one refused in an `error:abort` naming its hook, its
/// line's green unmoved and its store's index without the changeset; the first answered with the reply 7.2's server
/// wrote, byte for byte, judged over the served view of its store, `default`'s line at the pushed changeset and its
/// store's index holding it.
#[test]
fn a_push_through_the_driver_lands_in_the_repositorys_store_unless_a_hook_waits() {
  crate::test_store::audit_on_stored_shard("hg", |state| {
    let mut driven = Hg::new(state, "driven");
    let made = rebuild(state, &mut driven, (&[1], 1));
    let mut hooked = Hg::new(state, "hooked");
    rebuild(state, &mut hooked, (&[1], 1));
    set_hook(state, "hooked", "pretxnchangegroup.check");
    let (principal, heads) = (owner(), HeadsArgument::Force);
    let pushing = (&principal, &heads, &fixture!("push-plain.hg")[..]);
    let version = state.db.partition().green_head(hooked.root);
    let refused = drive(state, hooked.repository, pushing, &mut room());
    let message = b"the repository's pretxnchangegroup.check hook cannot run here yet: its view of the push is \
                    not served"
      .to_vec();
    assert_eq!(
      refused,
      Some(Outcome::refused(&Refused::Abort {
        message,
        hint: None
      }))
    );
    assert_eq!(state.db.partition().green_head(hooked.root), version);
    assert!(!hooked.holds(state, &made[3].node));
    let accepted = drive(state, driven.repository, pushing, &mut room());
    let Some(Outcome::Accepted(reply)) = accepted else {
      panic!("not accepted: {accepted:?}");
    };
    assert_eq!(encoded(&reply), fixture!("push-plain-reply.hg").to_vec());
    assert_eq!(head(state, driven.root), Some(made[3].node));
    assert!(driven.holds(state, &made[3].node));
  });
}

/// The push driver's steps in one call, for a test on a shard (the daemon's driver, `super::unbundle`, takes them a
/// slice at a time between yields): `heads` judged and the bundle read, the changegroup taken whole, the push landed on
/// the shard's store first and then judged and landed ([`super::land_in`], `super::land_stored`'s order, the one landing
/// path).
fn drive(
  state: &mut ShardState,
  repository: RepositoryId,
  (principal, heads, bundle): super::Pushing<'_>,
  room: &mut Admission,
) -> Option<Outcome> {
  let fronted = crate::vcs_store::lent_state(state, |state| {
    super::front(state, repository, (heads, bundle), room)
  });
  let mut unbundle = match fronted? {
    Ok(unbundle) => unbundle,
    Err(outcome) => return Some(outcome),
  };
  let forked = match unbundle.take_changegroup() {
    None => None,
    Some(changegroup) => match super::take_whole(state, repository, changegroup, room)? {
      Ok(forked) => Some(forked),
      Err(refused) => return Some(Outcome::refused(&refused)),
    },
  };
  super::land_in(
    state,
    repository,
    (principal, bundle),
    &unbundle,
    forked,
    room,
  )
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
  crate::test_store::audit_on_stored_shard("hg", |state| {
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
      Err(Box::new(Refused::Abort {
        message: denied.into_bytes(),
        hint: None
      }))
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
      Err(Box::new(Refused::Abort {
        message: listed.into_bytes(),
        hint: None
      }))
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
  crate::test_store::audit_on_stored_shard("hg", |state| {
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
      Err(Box::new(Refused::Abort {
        message: denied.into_bytes(),
        hint: None
      }))
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
  crate::test_store::audit_on_stored_shard("hg", |state| {
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
      Err(Box::new(Refused::Abort {
        message: denied.into_bytes(),
        hint: None
      }))
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
      hg_publishing: true,
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

/// A-694 (bug: an append's new greens were minted one id). Do: on the rebuilt repository, push a `bookmarks` part
/// making bookmarks `left` and `right` at `second`, the changeset `default`'s line and bookmark `main` hold. Expect the
/// push accepted, each new bookmark a green of its own whose head is `second`, and the three bookmarks' greens
/// different. Before, the second new green was minted the first's id and the push was answered `error:abort`.
#[test]
fn t_a694_a_push_making_two_bookmarks_makes_two_greens() {
  crate::test_store::audit_on_stored_shard("hg", |state| {
    let mut hg = Hg::new(state, "marks");
    let made = rebuild(state, &mut hg, (&[1], 1));
    let view = View::of(
      listing!("push-plain-server.txt"),
      listing!("push-plain-bookmarks.txt"),
    );
    let at = |name: &[u8]| BookmarkAt {
      name: name.to_vec(),
      node: Some(made[1].node),
    };
    let unbundle = Unbundle {
      parts: vec![taken(0, Asks::Bookmarks(vec![at(b"left"), at(b"right")]))],
    };
    let answer = hg.push(state, &unbundle, &view);
    assert!(answer.is_ok(), "{answer:?}");
    let bound = |name: &[u8]| hg.bound(state, &bookmark_name(name));
    let (main, left, right) = (bound(b"main"), bound(b"left"), bound(b"right"));
    let greens: BTreeSet<DbVolumeId> = [main, left, right].into_iter().flatten().collect();
    assert_eq!(greens.len(), 3, "{main:?} {left:?} {right:?}");
    for green in [left, right].into_iter().flatten() {
      assert_eq!(head(state, green), Some(made[1].node));
    }
  });
}

std::thread_local! {
  /// The task holding a run [`a_dropped_run_is_forgotten_and_the_next_push_lands_past_it`] cancels.
  static RUN_TASK: core::cell::Cell<Option<slates_rt::TaskId>> = const { core::cell::Cell::new(None) };
  /// Whether that task's stand-in changeset landed on the store.
  static STAND_IN_LANDED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// A push's run, as [`a_dropped_run_is_forgotten_and_the_next_push_lands_past_it`] holds one: one changeset landed on
/// the node's store at repository `repository`'s next sequence (a segment's stand-in: a record no record of the
/// repository's and a payload), then held until the task is cancelled.
async fn run_held_until_dropped(repository: RepositoryId) {
  let mut run = super::super::hg_run::Run::new(repository);
  let landing = crate::state::with_state(|state| {
    let disk = state.vcs_disk.clone()?;
    let sequence = state.vcs_stores.next_changeset(repository)?;
    let header = slates_store::log::Header {
      tool: slates_store::log::Tool::Mercurial,
      repository: u128::from_be_bytes(repository.bytes),
      sequence,
      judged_over: sequence.saturating_sub(1),
      grant: 0,
      work: 0,
      time_ns: slates_rt::futures::now_ns(),
      sections: 0,
    };
    let parts = vec![
      (
        slates_store::log::SectionKind::Record,
        b"abandoned".to_vec(),
      ),
      (
        slates_store::log::SectionKind::Payload,
        b"an abandoned segment".to_vec(),
      ),
    ];
    Some((disk, header, parts))
  })
  .flatten();
  let Some((disk, header, parts)) = landing else {
    return;
  };
  let sequence = header.sequence;
  if disk.land(header, parts).await.is_ok() {
    STAND_IN_LANDED.with(|landed| landed.set(true));
    run.landed(sequence, &[]);
  }
  std::future::pending::<()>().await;
  drop(run);
}

/// Polls the first shard of `daemon` with `look` until it answers true, or [`TASK_WAIT`] passes: whether it did.
fn until(
  daemon: &crate::daemon::Daemon,
  look: impl FnOnce(&mut ShardState) -> bool + Clone + Send + 'static,
) -> bool {
  let began = std::time::Instant::now();
  loop {
    if crate::daemon::observe_first(daemon, look.clone()) {
      return true;
    }
    if began.elapsed() > TASK_WAIT {
      return false;
    }
  }
}

/// The count of `name` on `state`.
fn counted(state: &ShardState, name: &str) -> u64 {
  state.refusals.get(name).copied().unwrap_or(0)
}

/// Main 01a20182's ruling of 2026-11-22 12:00 (an abandoned run's rows forgotten at its abandonment, counted and bounded;
/// a dropped future's included). Do: on a running daemon holding a changeset store, make a Mercurial repository; in a
/// task, land one changeset at its next sequence as a push's run does, then park; cancel the task; push two changesets
/// to it; make its store again from its log (the sliced `vcs_store::rebuild`). Expect the run counted abandoned once it
/// drops and its changeset's rows forgotten (its `C` and `P` rows at least); the push's run beginning past the abandoned
/// sequence, which no landing takes again while the daemon runs; and the rebuild the push's history.
#[test]
fn a_dropped_run_is_forgotten_and_the_next_push_lands_past_it() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (repository, root, named) = crate::daemon::observe_first(&daemon, |state| {
    let hg = Hg::new(state, "dropped");
    let named = state.vcs_stores.landed_of(hg.repository).unwrap();
    let task = slates_rt::futures::spawn(run_held_until_dropped(hg.repository)).unwrap();
    RUN_TASK.with(|held| held.set(Some(task)));
    (hg.repository, hg.root, named)
  });
  let landed = until(&daemon, move |state| {
    STAND_IN_LANDED.with(core::cell::Cell::get)
      && state.vcs_stores.landed_of(repository) == Some(named + 1)
  });
  crate::daemon::observe_first(&daemon, |_| {
    let task = RUN_TASK.with(core::cell::Cell::take).unwrap();
    slates_rt::futures::cancel(task).unwrap();
  });
  let forgotten = until(&daemon, |state| {
    counted(state, super::super::hg_run::RUN_ROWS_FORGOTTEN) >= 2
  });
  let (abandoned, (run, tip)) = crate::daemon::observe_first(&daemon, move |state| {
    let abandoned = counted(state, super::super::hg_run::RUNS_ABANDONED);
    let made = history_of(&[
      ("one", "one", b"one\n", None, "default"),
      ("two", "two", b"two\n", Some(0), "default"),
    ]);
    let unbundle = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[0, 1])))],
    };
    let mut hg = Hg { repository, root };
    let pushed = hg.push(state, &unbundle, &View::default());
    assert!(pushed.is_ok(), "{pushed:?}");
    let records = state.db.partition().repositories();
    let run = (0..)
      .map_while(|index| records.object_record(repository, index))
      .filter_map(slates_hg::import::record::run_of)
      .last();
    crate::vcs_store::rebuild(state, repository);
    (abandoned, (run, made[1].node))
  });
  let rebuilt = until(&daemon, move |state| {
    !state.vcs_stores.rebuilding(repository)
  });
  let head = crate::daemon::observe_first(&daemon, move |state| head(state, root));
  daemon.stop();
  assert!(landed, "the task's run landed a changeset at a sequence");
  assert!(forgotten, "the dropped run's rows forgotten");
  assert_eq!(abandoned, 1);
  let run = run.expect("the push's entry names its run");
  assert_eq!(
    run.first(),
    Some(named + 2),
    "the push's run past the abandoned sequence"
  );
  assert!(rebuilt, "still rebuilding after {TASK_WAIT:?}");
  assert_eq!(head, Some(tip), "the rebuild replayed the push's run");
}

/// Main 01a20182's (ii) of 2026-11-22 05:01 (w-interop's whole-history push of hg: its keeping fit its repository's
/// ceiling, but no step could run in what was left, and it waited for room that could never come). Do: on an audit
/// shard, its ceiling 1,000 bytes (`vcs_share::wait::TEST_CEILING`), size a push's need holding 100 bytes and keeping
/// 400 more, its widest whole text first 500 bytes, then 501. Expect the need admitted within the ceiling where keeping
/// and the widest text fit it (1,000), and the push refused in the typed words that it could never be taken
/// (`PAST_CEILING`) where they pass it by a byte. Red before: the second admitted the ceiling, its step left to wait.
#[test]
fn a_push_whose_keeping_and_its_widest_step_pass_its_repositorys_ceiling_is_refused_at_once() {
  crate::test_store::audit_on_stored_shard("hg-least-step", |state| {
    crate::vcs_share::wait::TEST_CEILING.with(|ceiling| ceiling.set(1_000));
    let fits = super::need_of(state, (100, 400, 500), 2_000);
    let passes = super::need_of(state, (100, 400, 501), 2_000);
    crate::vcs_share::wait::TEST_CEILING.with(|ceiling| ceiling.set(0));
    assert_eq!(fits.ok(), Some(1_000));
    assert_eq!(passes.err(), Some(super::past_ceiling()));
  });
}

/// A push's wait for room ends typed when its sleep is refused (main 01a20182's commit-2 list: the room wait dropped the
/// sleep's refusal, so it turned again at once and spun its shard through the wait's patience). Do: on a store-backed
/// daemon, make a Mercurial repository; from a task on its shard, poll a wait for room past the repository's room now,
/// with a waker that is no task's, so its sleep is refused (`RtError::NotOnShardThread`). Expect: the first poll
/// answers the wait ended by that refusal, within its first turn, counted once. Red before: the poll turned the wait
/// again at once for its whole patience, past the watchdog's window.
#[test]
fn a_rooms_wait_whose_sleep_is_refused_ends_at_once_with_the_refusal() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (id, _) = crate::test_store::create_on(
    &daemon,
    0,
    &owner(),
    ("room-wait", VcsTool::Mercurial, ObjectFormat::Sha1, "main"),
  );
  let (polled, spent, counted) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let room =
        crate::state::with_state(|state| crate::vcs_share::room(state, id).bytes).unwrap_or(0);
      let mut wait = std::pin::pin!(super::room_after_wait(id, room, None));
      let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
      let started = slates_rt::futures::now_ns();
      let polled = wait.as_mut().poll(&mut cx);
      let spent = slates_rt::futures::now_ns().saturating_sub(started);
      let counted = crate::state::with_state(|state| {
        state
          .refusals
          .get(super::INTAKE_ROOM_WAIT_SLEEP_REFUSED)
          .copied()
          .unwrap_or(0)
      })
      .unwrap_or(0);
      (polled, spent, counted)
    },
    TASK_WAIT,
  );
  daemon.stop();
  assert_eq!(
    polled,
    std::task::Poll::Ready(Some(Err(super::WaitEnded::SleepRefused(
      slates_rt::RtError::NotOnShardThread
    )))),
    "the wait ended by its sleep's refusal"
  );
  assert!(
    spent < crate::vcs_share::wait::PATIENCE_NS,
    "ended within its first turn, not after its patience: {spent} ns"
  );
  assert_eq!(counted, 1, "the refused sleep counted once");
}
