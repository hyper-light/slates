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
use slates_hg::push::reply::{Refused, Reply};
use slates_hg::push::{Asks, BookmarkAt, ChangegroupPart, Outcome, Taken, Unbundle, read};
use slates_hg::{Admission, NULL_NODE, Node, delta, node};
use slates_ipc::protocol::{ObjectFormat, ReplyBody, RequestBody, VcsTool};

use super::{
  BOOKMARK_TAG, LINE_TAG, bookmark_name, bookmark_of, branch_prefix, line_name, line_of,
  transaction_id,
};
use crate::state::ShardState;
use crate::verbs::dispatch;

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
#[derive(Clone, Copy, Debug)]
struct Hg {
  repository: RepositoryId,
  root: DbVolumeId,
}

impl Hg {
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

impl Hg {
  /// A new Mercurial repository `name`, its first line `default`'s, empty, made from a task on the shard: the creation
  /// awaited, as a client's is answered (`test_store::created`), never waited for on the shard's thread.
  async fn made(name: &str) -> Hg {
    let line = line_name(b"default", 0);
    let asked = (name, VcsTool::Mercurial, ObjectFormat::Sha1, line.as_str());
    let (repository, root) = crate::test_store::created(&owner(), asked).await;
    Hg { repository, root }
  }
}

impl Hg {
  /// [`Hg::made`], its policy then set not to publish (`hg_publishing` false, policy version 1), as the tests' own
  /// prepared pushes judged it (`Terms`' publishing false): a push's changesets stay drafts unless its parts publish
  /// them.
  async fn made_non_publishing(name: &str) -> Hg {
    let hg = Hg::made(name).await;
    let owned = name.to_owned();
    crate::repository::svn_commit::tests::on(move |state| {
      set_policy(state, (&owned, 0), Vec::new())
    });
    hg
  }
}

/// One part as [`bundled`] lays it out before encoding: its id, its kind, its parameters and its payload.
type BundledPart = (
  u32,
  &'static [u8],
  Vec<slates_hg::bundle2::Param<'static>>,
  Vec<u8>,
);

/// `unbundle`'s parts as the bundle2 a client sends: its replycaps, check:heads, changegroup, phase-heads and bookmarks
/// parts, each mandatory, in order, each under its own id (a reply names a part by it). A part of another kind is no push these tests send whole.
fn bundled(unbundle: &Unbundle) -> Vec<u8> {
  use slates_hg::bundle2::{Event, Param, Part};
  let mut payloads: Vec<BundledPart> = Vec::new();
  for part in &unbundle.parts {
    match &part.asks {
      Asks::ReplyCaps(caps) => payloads.push((part.id, b"REPLYCAPS", Vec::new(), caps.clone())),
      Asks::CheckHeads(heads) => {
        let mut payload = Vec::new();
        slates_hg::heads::encode(heads, &mut payload);
        payloads.push((part.id, b"CHECK:HEADS", Vec::new(), payload));
      }
      Asks::Changegroup(group) => {
        assert_eq!(group.version, Version::V03, "a version-03 changegroup");
        let version = Param {
          key: b"version",
          value: b"03",
        };
        payloads.push((
          part.id,
          b"CHANGEGROUP",
          vec![version],
          group.payload.clone(),
        ));
      }
      Asks::PhaseHeads(heads) => {
        let mut payload = Vec::new();
        slates_hg::phases::encode(heads, &mut payload);
        payloads.push((part.id, b"PHASE-HEADS", Vec::new(), payload));
      }
      Asks::Bookmarks(marks) => {
        let marks: Vec<_> = marks
          .iter()
          .map(|mark| slates_hg::bookmarks::Bookmark {
            name: &mark.name,
            node: mark.node,
          })
          .collect();
        let mut payload = Vec::new();
        slates_hg::bookmarks::encode(&marks, &mut payload).expect("a bookmarks payload");
        payloads.push((part.id, b"BOOKMARKS", Vec::new(), payload));
      }
      other => panic!("no bundle2 written for {other:?}"),
    }
  }
  let parts = payloads
    .iter()
    .map(|(id, kind, params, _)| Part {
      kind,
      id: *id,
      mandatory_params: params.len(),
      params: params.clone(),
      depth: 0,
    })
    .collect();
  let events = payloads
    .iter()
    .enumerate()
    .flat_map(|(at, (_, _, _, payload))| [Event::Open(at), Event::Chunk(payload), Event::Close])
    .collect();
  let bundle = Bundle2 {
    params: Vec::new(),
    parts,
    events,
  };
  let mut out = Vec::new();
  bundle.encode(&mut out).expect("a bundle2");
  out
}

/// `unbundle` pushed to `hg`'s repository through the daemon's driver from a task on its shard
/// ([`super::unbundle`]): every store wait awaited, never the shard's thread blocked. What it came to.
async fn pushed(hg: &Hg, unbundle: &Unbundle) -> Outcome {
  pushed_bytes(hg, &bundled(unbundle)).await
}

/// `bundle`, a bundle2 as a client sends it, pushed as [`pushed`] pushes one.
async fn pushed_bytes(hg: &Hg, bundle: &[u8]) -> Outcome {
  pushed_bytes_to(hg.repository, bundle).await
}

/// `bundle` pushed to repository `id` by the tests' owner through the daemon's driver ([`super::unbundle`]), from a
/// task on its shard: what it came to.
async fn pushed_bytes_to(id: RepositoryId, bundle: &[u8]) -> Outcome {
  super::unbundle(id, (&owner(), &HeadsArgument::Force, bundle), &mut room())
    .await
    .expect("a served view")
}

/// `part` pushed alone to repository `id` ([`pushed_bytes_to`]), from a task on its shard, accepted: as a client's push of
/// the changegroup lands it, every store wait awaited.
pub(crate) async fn pushed_changegroup(id: RepositoryId, part: ChangegroupPart) {
  let unbundle = Unbundle {
    parts: vec![taken(0, Asks::Changegroup(part))],
  };
  let outcome = pushed_bytes_to(id, &bundled(&unbundle)).await;
  assert!(
    matches!(outcome, Outcome::Accepted(_)),
    "the push was refused: {outcome:?}"
  );
}

/// Ruling R, piece (i) (main 01a20182's ruling of 2026-11-26: a push's changeset is homed in the region, its record
/// naming one homed list, and no record names content not durable at its homes, A-727's rule). Do: start a daemon over
/// an anchor segment that holds no content object, so no shard of it can publish an image (`verbs::publish_shard`
/// refuses `RecoveryIncomplete`) and no home can make what it keeps durable; push two changesets to a Mercurial
/// repository there. Expect: the push refused in slates' words that its changeset was not made durable at its homes
/// (`HOMED_SHORT`), its homing counted short (`vcs.hg.push_homed_short`), nothing of it in the repository's register,
/// and nothing held for the region once its attempt's holds are settled. Red before: the push landed and its entry
/// committed, naming segments no home held.
#[test]
fn a_push_whose_changeset_is_not_durable_at_its_homes_is_refused_and_records_nothing() {
  use crate::repository::store_tests::{anchor_of, daemon_over};
  let profile = crate::daemon::test_profile();
  let instance = format!("hg-unhomed-{}", std::process::id());
  let store = crate::test_store::TestStore::start("hg-unhomed");
  let anchor = anchor_of(&instance);
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  daemon.bootstrap(true).unwrap();
  let hg = made_on(&daemon, "unhomed");
  crate::daemon::observe_first(&daemon, |state| {
    crate::verbs::publish_shard(state).unwrap();
  });
  daemon.stop();
  // Its successor over the same segment, its anchor's content object gone: its shards keep no recovery image.
  let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(1));
  config.vcs_disk = Some(store.handle());
  let (handoff, len) = anchor.0.handoff().unwrap();
  let source = crate::daemon::SegmentSource::Handoff {
    handoff,
    len,
    content: None,
    rendezvous: anchor.1.adoption(),
  };
  let daemon = crate::Daemon::start(&profile, config, source).unwrap();

  let repository = hg.repository;
  let made = history_of(&[
    ("one", "one", b"one\n", None, "default"),
    ("two", "two", b"two\n", Some(0), "default"),
  ]);
  let bundle = crate::vcs_http::daemon_edge_tests::history_bundle(&made, &[0, 1]);
  let outcome = crate::test_store::run_on_first(
    &daemon,
    move || async move { pushed_bytes_to(repository, &bundle).await },
    NAMED_PATIENCE,
  );
  let settled = until(&daemon, |_| {
    crate::vcs_store::homes::region_held_here().is_empty()
  });
  let (records, short) = crate::daemon::observe_first(&daemon, move |state| {
    let records = (0..)
      .map_while(|index| {
        state
          .db
          .partition()
          .repositories()
          .object_record(repository, index)
      })
      .count();
    let short = state
      .refusals
      .get(super::super::hg_run::HOMED_SHORT)
      .copied()
      .unwrap_or(0);
    (records, short)
  });
  daemon.stop();
  let words = match &outcome {
    Outcome::Refused(reply) => encoded(reply),
    other => panic!("the push was not refused: {other:?}"),
  };
  assert!(
    words
      .windows(super::HOMED_SHORT.len())
      .any(|at| at == super::HOMED_SHORT),
    "refused in slates' words for a changeset not durable at its homes: {}",
    String::from_utf8_lossy(&words)
  );
  assert!(short > 0, "its homing counted short");
  assert_eq!(records, 0, "nothing of the push in the register");
  assert!(
    settled,
    "nothing held for the region once its holds settled"
  );
}

/// Unit 43 (main 01a20182's r42 of 2026-11-26: a restart's one unit applied a whole tree's chain entry in one step,
/// 329.6 ms of CPU at 6a4dcbc6a, 952.7 ms at 20a49642c). Do: on a stored shard push a line of [`WIDE_TREE`] changesets
/// each adding a file, so `default`'s green records one entry naming every file; replay that green's chain as a
/// restart does with each apply step given one nanosecond (`verbs::TEST_REPLAY_APPLY_BUDGET_NS`). Expect: the replay
/// ends, its one entry applied in more steps than one. Red before: an entry's apply was one `apply_recorded` call, a
/// step an entry.
#[test]
fn a_restarts_replay_applies_a_whole_trees_entry_a_slice_at_a_time() {
  let (made, all) = wide_history(WIDE_TREE, 0);
  let bundle = crate::vcs_http::daemon_edge_tests::history_bundle(&made, &all);
  let steps = crate::test_store::run_on_stored_shard(move || async move {
    let hg = Hg::made("sliced-apply").await;
    let outcome = pushed_bytes_to(hg.repository, &bundle).await;
    assert!(matches!(outcome, Outcome::Accepted(_)), "{outcome:?}");
    crate::repository::svn_commit::tests::on(|state| {
      crate::verbs::TEST_REPLAY_APPLY_BUDGET_NS.with(|budget| budget.set(Some(1)));
      let steps = crate::verbs::replay_apply_steps_for_test(state, hg.root);
      crate::verbs::TEST_REPLAY_APPLY_BUDGET_NS.with(|budget| budget.set(None));
      (steps, state.db.partition().green_head(hg.root))
    })
  });
  let (steps, entries) = steps;
  let steps = steps.expect("the green's replay ended");
  println!("unit 43: {entries} entries applied in {steps} steps");
  assert!(
    steps > entries,
    "the entry's apply sliced: {steps} steps for {entries} entries"
  );
}

/// Ruling R piece (ii)'s oracle (main 01a20182's ruling of 2026-11-26: the segment homed as it landed, and a holder's
/// landed segment the owner's byte for byte, by identity). Do: on a stored shard, cut a push's segments at one byte and
/// push four changesets; then make its run whole on a second node's store that holds none of it, as a holder's rebuild
/// does (`vcs_store::homed_run_on_store`), and again. Expect: the list and every segment fetched from the region once
/// (`vcs_store.hg_run_fetched` the segments and the list), the second pass fetching nothing; and on the second store
/// the list and each segment's `Record`, `Payload` and `Texts`, read by identity, the owner's byte for byte. Red before:
/// a holder's rebuild read its own store alone and was refused `Unlanded`.
#[test]
fn a_holders_run_lands_the_owners_segments_byte_for_byte_by_identity() {
  let holder = crate::test_store::TestStore::start("hg-holder-store");
  let theirs = holder.handle();
  let (fetched, again, entries, differing) =
    crate::test_store::run_on_stored_shard(move || async move {
      let hg = Hg::made("fetched").await;
      let made = history_of(&[
        ("one", "one", b"one\n", None, "default"),
        ("two", "two", b"two\n", Some(0), "default"),
        ("three", "three", b"three\n", Some(1), "default"),
        ("four", "four", b"four\n", Some(2), "default"),
      ]);
      super::super::hg_run::TEST_SEGMENT_BYTES.with(|bound| bound.set(1));
      let outcome = pushed_bytes_to(
        hg.repository,
        &crate::vcs_http::daemon_edge_tests::history_bundle(&made, &[0, 1, 2, 3]),
      )
      .await;
      super::super::hg_run::TEST_SEGMENT_BYTES.with(|bound| bound.set(0));
      assert!(matches!(outcome, Outcome::Accepted(_)), "{outcome:?}");
      let (homed, ours) = crate::repository::svn_commit::tests::on(|state| {
        let records = state.db.partition().repositories();
        let homed = (0..)
          .map_while(|index| records.object_record(hg.repository, index))
          .filter_map(slates_hg::import::record::homed_run_of)
          .last();
        (
          homed.expect("the push's homed run"),
          state.vcs_disk.clone().expect("a store"),
        )
      });
      let count = || {
        crate::repository::svn_commit::tests::on(|state| {
          state
            .refusals
            .get(crate::vcs_store::HG_RUN_FETCHED)
            .copied()
            .unwrap_or(0)
        })
      };
      let before = count();
      let ran = crate::vcs_store::homed_run_on_store(&theirs, hg.repository, homed).await;
      let fetched = count() - before;
      assert!(ran.is_some(), "the run made whole on the second store");
      let again = crate::vcs_store::homed_run_on_store(&theirs, hg.repository, homed).await;
      let again = (again.is_some(), count() - before - fetched);
      let read = async |disk: &slates_store::owner::StoreHandle,
                        (identity, len): ([u8; 32], u64)| {
        crate::vcs_store::keyed_bytes(disk, identity, len).await
      };
      let list = read(&ours, (homed.list, homed.len))
        .await
        .expect("the owner's list");
      let mut differing = Vec::new();
      if read(&theirs, (homed.list, homed.len)).await.as_deref() != Some(&list[..]) {
        differing.push("the list".to_owned());
      }
      let entries = slates_hg::import::record::segment_list_of(&list).expect("a list");
      for entry in &entries {
        for (name, section) in [
          ("record", entry.record),
          ("payload", entry.payload),
          ("texts", entry.texts),
        ] {
          let owners = read(&ours, section).await;
          if owners.is_none() || read(&theirs, section).await != owners {
            differing.push(format!("segment {}'s {name}", entry.sequence));
          }
        }
      }
      (fetched, again, entries.len(), differing)
    });
  drop(holder);
  assert!(entries >= 3, "several segments: {entries}");
  assert_eq!(
    fetched,
    u64::try_from(entries).unwrap() + 1,
    "each segment and the list fetched once"
  );
  assert_eq!(again, (true, 0), "a second pass fetches nothing");
  assert!(
    differing.is_empty(),
    "the holder's sections differ from the owner's: {differing:?}"
  );
}

/// The settle queue's shape (main 01a20182's order of 2026-11-26 after ruling R piece (i): an unbounded queue is CLAUDE.md
/// §2 item 8). Do: on a stored shard, cut a push's segments at one byte so each of its six changesets lands as segments
/// of their own, and push it. Expect: the push accepted, landed as several segments (each homing its `Record` and its
/// `Payload`, and its list), and its attempt's end owed the region as one settle carrying every object it homed
/// (`vcs_store.homes.settles_queued` 1). Red before: one settle an object homed, two a segment and one the list.
#[test]
fn a_many_segment_pushs_attempt_settles_its_homed_objects_as_one() {
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = Hg::made("settled-once").await;
    let made = history_of(&[
      ("one", "one", b"one\n", None, "default"),
      ("two", "two", b"two\n", Some(0), "default"),
      ("three", "three", b"three\n", Some(1), "default"),
      ("four", "four", b"four\n", Some(2), "default"),
      ("five", "five", b"five\n", Some(3), "default"),
      ("six", "six", b"six\n", Some(4), "default"),
    ]);
    super::super::hg_run::TEST_SEGMENT_BYTES.with(|bound| bound.set(1));
    let count = |name: &str| {
      crate::repository::svn_commit::tests::on(|state| {
        state.refusals.get(name).copied().unwrap_or(0)
      })
    };
    let queued = count(crate::vcs_store::homes::SETTLES_QUEUED);
    let landed = count(super::super::hg_run::SEGMENTS_LANDED);
    let outcome = pushed_bytes_to(
      hg.repository,
      &crate::vcs_http::daemon_edge_tests::history_bundle(&made, &[0, 1, 2, 3, 4, 5]),
    )
    .await;
    super::super::hg_run::TEST_SEGMENT_BYTES.with(|bound| bound.set(0));
    assert!(matches!(outcome, Outcome::Accepted(_)), "{outcome:?}");
    let segments = count(super::super::hg_run::SEGMENTS_LANDED) - landed;
    assert!(segments >= 3, "landed as several segments: {segments}");
    assert_eq!(
      count(crate::vcs_store::homes::SETTLES_QUEUED) - queued,
      1,
      "one settle for the attempt's {segments} segments and its list"
    );
  });
}

/// The settle queue's bound (main 01a20182's order of 2026-11-26: a derived bound, a typed refusal at it, a counter).
/// Do: on a stored shard, begin as many attempts as the shard holds at once (`homes::settle_bound`), then push a
/// changeset; end those attempts and push it again. Expect: the first push refused in slates' words that the server has
/// as many pushes in flight as it holds (`PUSHES_AT_BOUND`), counted (`vcs_store.homes.attempts_at_bound`), nothing of
/// it in the register; the second accepted. Red before: no bound, so the attempts and the settles they owe grew without
/// one.
#[test]
fn a_push_past_the_shards_attempts_bound_is_refused_typed_and_the_next_lands() {
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = Hg::made("at-bound").await;
    let made = history_of(&[("one", "one", b"one\n", None, "default")]);
    let bundle = crate::vcs_http::daemon_edge_tests::history_bundle(&made, &[0]);
    let held: Vec<slates_cluster::homes::HomeOwner> =
      crate::repository::svn_commit::tests::on(|state| {
        let bound = crate::vcs_store::homes::settle_bound(state);
        (0..bound)
          .map_while(|_| crate::vcs_store::homes::attempt_owner(state, hg.repository.bytes).ok())
          .collect()
      });
    let refused = pushed_bytes_to(hg.repository, &bundle).await;
    let (counted, records) = crate::repository::svn_commit::tests::on(|state| {
      let counted = state
        .refusals
        .get(crate::vcs_store::homes::ATTEMPTS_AT_BOUND)
        .copied()
        .unwrap_or(0);
      let records = (0..)
        .map_while(|index| {
          state
            .db
            .partition()
            .repositories()
            .object_record(hg.repository, index)
        })
        .count();
      (counted, records)
    });
    for owner in &held {
      crate::vcs_store::homes::attempt_ended(owner);
    }
    let accepted = pushed_bytes_to(hg.repository, &bundle).await;
    let words = match &refused {
      Outcome::Refused(reply) => encoded(reply),
      other => panic!("the push past the bound was not refused: {other:?}"),
    };
    assert!(
      words
        .windows(super::PUSHES_AT_BOUND.len())
        .any(|at| at == super::PUSHES_AT_BOUND),
      "{}",
      String::from_utf8_lossy(&words)
    );
    assert!(counted > 0, "the refusal counted");
    assert_eq!(records, 0, "nothing of the refused push recorded");
    assert!(matches!(accepted, Outcome::Accepted(_)), "{accepted:?}");
  });
}

/// Ruling R, its first piece's counter (main's order of 2026-11-26: a segment's bytes by section, read from the status
/// report). Do: on a stored shard push three changesets, each adding a file. Expect: the push accepted and its segments'
/// landed bytes counted by section: the `Payload` bytes exactly the bytes its deltas' chunks took
/// (`vcs.hg.segment_compress_stored_bytes`, counted as the chunks were made), and the `Record` and `Texts` bytes moved.
/// Red before: nothing counted a landed segment's sections.
#[test]
fn a_pushs_landed_segments_are_counted_by_section() {
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = Hg::made("sections").await;
    let made = history_of(&[
      ("base", "a", b"alpha\n", None, "default"),
      ("second", "b", b"beta\n", Some(0), "default"),
      ("third", "c", b"gamma\n", Some(1), "default"),
    ]);
    let outcome = pushed_bytes_to(
      hg.repository,
      &crate::vcs_http::daemon_edge_tests::history_bundle(&made, &[0, 1, 2]),
    )
    .await;
    assert!(matches!(outcome, Outcome::Accepted(_)), "{outcome:?}");
    let counted = crate::repository::svn_commit::tests::on(|state| {
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      (
        count(crate::repository::hg_run::LANDED_RECORD_BYTES),
        count(crate::repository::hg_run::LANDED_PAYLOAD_BYTES),
        count(crate::repository::hg_run::LANDED_TEXTS_BYTES),
        count(super::COMPRESS_STORED_BYTES),
      )
    });
    let (record, payload, texts, chunked) = counted;
    assert_eq!(payload, chunked, "the payload is its chunks: {counted:?}");
    assert!(
      record > 0 && payload > 0 && texts > 0,
      "each section counted: {counted:?}"
    );
  });
}

/// Bookmark `mark` pushed to repository `id` at `node` alone (a `bookmarks` part, as `hg push -B` sends one for a
/// changeset the server holds): the push's answer.
pub(crate) async fn pushed_bookmark(id: RepositoryId, mark: &[u8], node: Node) -> Outcome {
  let unbundle = Unbundle {
    parts: vec![taken(
      0,
      Asks::Bookmarks(vec![BookmarkAt {
        name: mark.to_vec(),
        node: Some(node),
      }]),
    )],
  };
  pushed_bytes_to(id, &bundled(&unbundle)).await
}

/// Mercurial repository `name` made on `daemon`'s first shard and `unbundle` pushed to it ([`pushed`]), from a task
/// there, accepted: the repository.
fn made_and_pushed_on(daemon: &crate::Daemon, name: &'static str, unbundle: Unbundle) -> Hg {
  let (hg, outcome) = crate::test_store::run_on_first(
    daemon,
    move || async move {
      let hg = Hg::made(name).await;
      let outcome = pushed(&hg, &unbundle).await;
      (hg, outcome)
    },
    NAMED_PATIENCE,
  );
  assert!(
    matches!(outcome, Outcome::Accepted(_)),
    "the push was refused: {outcome:?}"
  );
  hg
}

/// Mercurial repository `name` made on `daemon`'s first shard, not publishing ([`Hg::made_non_publishing`]), and
/// `unbundle` pushed to it as a client pushes it ([`pushed`]), from a task there, every store wait awaited, accepted:
/// the repository's id. For another module's daemon test that needs a history landed draft before its own steps.
pub(crate) fn made_non_publishing_and_pushed_on(
  daemon: &crate::Daemon,
  name: &'static str,
  unbundle: Unbundle,
) -> RepositoryId {
  let (hg, outcome) = crate::test_store::run_on_first(
    daemon,
    move || async move {
      let hg = Hg::made_non_publishing(name).await;
      let outcome = pushed(&hg, &unbundle).await;
      (hg, outcome)
    },
    NAMED_PATIENCE,
  );
  assert!(
    matches!(outcome, Outcome::Accepted(_)),
    "the push was refused: {outcome:?}"
  );
  hg.repository
}

/// Mercurial repository `name` made on `daemon`'s first shard from a task there (`test_store::created`).
fn made_on(daemon: &crate::Daemon, name: &'static str) -> Hg {
  crate::test_store::run_on_first(
    daemon,
    move || async move { Hg::made(name).await },
    NAMED_PATIENCE,
  )
}

/// `bundle`, a bundle2 as a client sends it, pushed to `hg` from a task on `daemon`'s first shard ([`pushed_bytes_to`]),
/// accepted.
fn pushed_bundle_on(daemon: &crate::Daemon, hg: &Hg, bundle: Vec<u8>) {
  let repository = hg.repository;
  let outcome = crate::test_store::run_on_first(
    daemon,
    move || async move { pushed_bytes_to(repository, &bundle).await },
    NAMED_PATIENCE,
  );
  assert!(
    matches!(outcome, Outcome::Accepted(_)),
    "the push was refused: {outcome:?}"
  );
}

/// `unbundle` pushed to `hg` ([`pushed`]) from a task on `daemon`'s first shard, accepted.
fn pushed_unbundle_on(daemon: &crate::Daemon, hg: Hg, unbundle: Unbundle) {
  let outcome = crate::test_store::run_on_first(
    daemon,
    move || async move { pushed(&hg, &unbundle).await },
    NAMED_PATIENCE,
  );
  assert!(
    matches!(outcome, Outcome::Accepted(_)),
    "the push was refused: {outcome:?}"
  );
}

/// Shape: how long a race history waits for a push it spawned to reach its held landing or to answer.
const RACE_PATIENCE_NS: u64 = 60_000_000_000;

/// `bundle` pushed to `hg` from a task of its own on this shard ([`pushed_bytes`]): its answer arrives on the receiver.
fn spawned_push(hg: Hg, bundle: Vec<u8>) -> std::sync::mpsc::Receiver<Outcome> {
  let (sent, answer) = std::sync::mpsc::sync_channel(1);
  let task = slates_rt::futures::spawn(async move {
    let _ = sent.send(pushed_bytes(&hg, &bundle).await);
  })
  .expect("the push's task");
  slates_rt::futures::detach(task).expect("detached");
  answer
}

/// The answer `answer` brings, waited for a turn at a time within [`RACE_PATIENCE_NS`].
async fn answered(answer: &std::sync::mpsc::Receiver<Outcome>) -> Outcome {
  let deadline = slates_rt::futures::now_ns().saturating_add(RACE_PATIENCE_NS);
  loop {
    if let Ok(outcome) = answer.try_recv() {
      return outcome;
    }
    assert!(
      slates_rt::futures::now_ns() < deadline,
      "the push never answered"
    );
    slates_rt::futures::yield_now().await;
  }
}

/// Waits a turn at a time, within [`RACE_PATIENCE_NS`], until a push holds at its landing (`hg_push::landing_held`).
async fn until_held() {
  let deadline = slates_rt::futures::now_ns().saturating_add(RACE_PATIENCE_NS);
  while !super::landing_held() {
    assert!(
      slates_rt::futures::now_ns() < deadline,
      "no push reached its landing"
    );
    slates_rt::futures::yield_now().await;
  }
}

/// [`pushed_bytes`] from a task on `daemon`'s first shard, accepted.
fn pushed_on(daemon: &crate::Daemon, hg: Hg, bundle: &'static [u8]) {
  let outcome = crate::test_store::run_on_first(
    daemon,
    move || async move { pushed_bytes(&hg, bundle).await },
    NAMED_PATIENCE,
  );
  assert!(matches!(outcome, Outcome::Accepted(_)), "{outcome:?}");
}

/// [`rebuild`] through the daemon's driver ([`pushed`]): the fixtures' history, accepted.
async fn rebuilt(hg: &Hg, (public, mark): (&[usize], usize)) -> Vec<Made> {
  let (made, unbundle) = rebuilding((public, mark));
  let outcome = pushed(hg, &unbundle).await;
  assert!(
    matches!(outcome, Outcome::Accepted(_)),
    "the rebuild was refused: {outcome:?}"
  );
  made
}

/// Mercurial repository `name` made on `daemon`'s first shard and the fixtures' history pushed to it
/// ([`rebuilt`]), from a task there: the repository and the history.
fn rebuilt_on(
  daemon: &crate::Daemon,
  name: &'static str,
  (public, mark): (&'static [usize], usize),
) -> (Hg, Vec<Made>) {
  crate::test_store::run_on_first(
    daemon,
    move || async move {
      let hg = Hg::made(name).await;
      let made = rebuilt(&hg, (public, mark)).await;
      (hg, made)
    },
    NAMED_PATIENCE,
  )
}

/// A line's head as the server reads it (`hg_made::head_of`): its root's attribute, or the head the push a new line owes
/// sets (P2), read without making it.
fn head(state: &ShardState, green: DbVolumeId) -> Option<Node> {
  crate::vcs_store::hg_made::head_of(state, green)
}

/// A green's file at its head.
fn file(state: &ShardState, green: DbVolumeId, path: &str) -> Option<Vec<u8>> {
  crate::vcs_store::named_objects::read_file(state, green, path)
}

/// [`rebuild`]'s push: the fixtures' history and the unbundle that makes it.
fn rebuilding((public, mark): (&[usize], usize)) -> (Vec<Made>, Unbundle) {
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
  (made, unbundle)
}

/// The rebuilt repository against 7.2's: every node 7.2's, `default`'s first line at `second`, and the index holding both
/// changesets ([`assert_rebuilt_awaited`] reads its files).
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
  assert!(hg.holds(state, &made[0].node) && hg.holds(state, &made[1].node));
}

/// [`assert_rebuilt`] from a task on the shard, and the root's `b` read through its name as a mount reads it
/// (`named_objects::read_file_brought`): its text let go once its push landed, as production lets one go.
async fn assert_rebuilt_awaited(hg: &Hg, made: &[Made]) {
  crate::repository::svn_commit::tests::on(|state| assert_rebuilt(state, hg, made));
  let read = crate::vcs_store::named_objects::read_file_brought(hg.root, "b").await;
  assert_eq!(read.as_deref(), Some(&b"b\n"[..]));
}

/// A-887 (main 01a20182's order of 2026-11-23 on the greens' 56 MB: the 57k push's `Green::make_mapped` bytes, live at
/// its end). Do: rebuild the fixtures' repository as one push to its empty first line, then look at every file of its
/// root green's head. Expect: each file whose bytes the repository's content store holds whole (by their identity) held
/// by that name (`Content::Named`), none of them as bytes, and some such files (non-vacuity). Failing first: every one
/// held as bytes, the green's second copy of what the store holds.
#[test]
fn a_pushed_file_the_content_store_holds_whole_is_held_by_its_name() {
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = Hg::made_non_publishing("named").await;
    let _ = rebuilt(&hg, (&[1], 1)).await;
    crate::repository::svn_commit::tests::on(|state| {
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
  let (named_away, served) = crate::test_store::run_on_first(
    &daemon,
    || async move {
      let hg = Hg::made("named-read").await;
      let _ = rebuilt(&hg, (&[1], 1)).await;
      let named_away = crate::state::with_state(|state| named_away_at(state, hg.root, "b"));
      let served = crate::vcs_store::named_objects::read_file_brought(hg.root, "b").await;
      (named_away.flatten(), served)
    },
    NAMED_PATIENCE,
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

/// Whether green `green` holds `path` by its object's name with that object let go of the content store; none where it
/// holds no such file.
fn named_away_at(state: &ShardState, green: DbVolumeId, path: &str) -> Option<bool> {
  let content = state.vcs_stores.content()?;
  state
    .greens
    .get(&green)?
    .content(path)
    .map(|held| match held {
      slates_merge::engine::Content::Named(identity) => {
        content.is_away(&slates_vcs::ContentId(identity.hash))
      }
      slates_merge::engine::Content::Bytes(_) => false,
    })
}

/// The reply recorded for request `id` of `origin`, waited for a step at a time within [`NAMED_PATIENCE`]. A parked
/// request whose bring-back was refused is answered unrecorded (`verbs::park_on_away`), so the wait fails at once on
/// one (`verbs.bring_back_refused`).
async fn recorded_answer(origin: u64, id: slates_wire::request::RequestId) -> ReplyBody {
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
/// (`verbs.parked_on_away`), and the work.
async fn edited_named(green: DbVolumeId) -> (ReplyBody, u64, slates_ipc::protocol::VolumeId) {
  edited_named_as(green, ("edit-named", 1)).await
}

/// [`edited_named`] by a work named `name`, its submit request `sequence` of client 7.
async fn edited_named_as(
  green: DbVolumeId,
  (name, sequence): (&str, u32),
) -> (ReplyBody, u64, slates_ipc::protocol::VolumeId) {
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
        name: name.to_owned(),
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
    sequence,
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
  (submitted, parked().saturating_sub(before), work)
}

/// A-887's owed gap (main 01a20182's order of 2026-11-24 04:40: a takeover's re-adoption of a Mercurial green brings back
/// the named bases its replay reads, by the planner a recovery runs). Do: on a store-backed daemon, rebuild the fixtures'
/// repository as one push, its root green holding `b` by its object's name; edit `b` in place over a work and submit it
/// (`edited_named`); let `b`'s base go again and leave the green as a restart leaves an adopted one (its engine gone,
/// awaiting its re-adoption through version 0, its repository's lineage gone with its every member, as a boot leaves it),
/// as `a_readopted_green_brings_back_the_texts_its_checkpoint_names_first`
/// leaves a Subversion one; re-adopt it over a fresh engine (`green_adopt::readopt_with`); then read `b` through its name
/// and edit it again over a second work. Expect: re-adopted, the base brought back by the planner
/// (`vcs.hg.named_recovery_brought_back` 1); `b` `bx\n`; the second edit landed, `b` `bxx\n`. Failing first: not
/// re-adopted, the replay of the edit fencing the green `ContentMissing`, the splice's base unread (`readopt_with` ran
/// Subversion's pre-pass alone). Failing second (main's order of 2026-11-24 05:51): the second edit refused `NotHead`,
/// its changeset not made (since 946484875).
#[test]
fn a_readopted_green_brings_back_the_named_base_its_chain_splices_first() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (hg, _) = rebuilt_on(&daemon, "named-readopt", (&[1], 1));
  let (green, base, repository) = crate::daemon::observe_first(&daemon, move |state| {
    let base = match state.greens.get(&hg.root).unwrap().content("b") {
      Some(slates_merge::engine::Content::Named(identity)) => identity.hash,
      other => panic!("`b` not named: {other:?}"),
    };
    (hg.root, base, hg.repository)
  });
  let (submitted, _, _) = crate::test_store::run_on_first(
    &daemon,
    move || async move { edited_named(green).await },
    NAMED_PATIENCE,
  );
  assert!(
    !matches!(submitted, ReplyBody::Refused { .. }),
    "{submitted:?}"
  );
  crate::daemon::observe_first(&daemon, move |state| {
    crate::vcs_store::name_holds::let_go_landed_texts(state, &[base]);
    assert!(
      state
        .vcs_stores
        .content()
        .is_some_and(|content| !content.holds_whole(&slates_vcs::ContentId(base))),
      "the base let go again"
    );
    // As a boot leaves the repository whose root awaits its re-adoption: no lineage, since each member is rebuilt over
    // the one it was made from, and one made from the root is fenced (`verbs::rebuild_member_sliced`).
    retire_lineage(state, repository);
    assert!(!state.greens.has_lineage(repository), "no lineage left");
    state.greens.remove(&green);
    state.merge.awaiting_adoption.insert(
      green,
      crate::green_adopt::AwaitingAdoption {
        through: 0,
        identity: None,
        replay: None,
      },
    );
  });
  let (readopted, brought, read, edited, after) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let shard = crate::state::with_state(|state| state.shard).unwrap();
      let readopted =
        crate::green_adopt::readopt_with(shard, shard, green, slates_merge::engine::Green::new())
          .await;
      let brought = crate::repository::svn_commit::tests::on(|state| {
        state
          .refusals
          .get(crate::vcs_store::hg_named::RECOVERY_BROUGHT_BACK)
          .copied()
          .unwrap_or(0)
      });
      if readopted != Some(true) {
        return (readopted, brought, None, None, None);
      }
      let read = crate::vcs_store::named_objects::read_file_brought(green, "b").await;
      let (edited, _, _) = edited_named_as(green, ("edit-named-again", 2)).await;
      let after = crate::vcs_store::named_objects::read_file_brought(green, "b").await;
      (readopted, brought, read, Some(edited), after)
    },
    NAMED_PATIENCE,
  );
  daemon.stop();
  assert_eq!(readopted, Some(true), "re-adopted");
  assert_eq!(brought, 1, "the splice's base brought back first");
  assert_eq!(read.as_deref(), Some(&b"bx\n"[..]), "the edited file");
  let edited = edited.expect("the second edit submitted");
  assert!(!matches!(edited, ReplyBody::Refused { .. }), "{edited:?}");
  assert_eq!(after.as_deref(), Some(&b"bxx\n"[..]), "edited again");
}

/// Main's ruling of 2026-11-24 on write-path bring-backs (w-hg's sweep: `hg_named::bring_back_named`'s group refused
/// partway left what it had brought out of its let-go). Do: on a store-backed daemon, rebuild the fixtures' repository
/// as one push, its root green holding `b` by its object's name; edit `b` in place over a work; let `b`'s base go; then
/// run the re-adoption's pre-pass (`hg_named::bring_back_named_live`) with the group refused once its base came back (a
/// store that gave it back short), and let go what it returns, as the re-adoption does. Expect the base among what it
/// returns, and not held after the let-go. Red before: nothing returned for the refused group, the base held for good.
#[test]
fn a_named_group_refused_partway_lets_go_what_it_brought_back() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (hg, _) = rebuilt_on(&daemon, "named-partial", (&[1], 1));
  let (green, base) = crate::daemon::observe_first(&daemon, move |state| {
    let base = match state.greens.get(&hg.root).unwrap().content("b") {
      Some(slates_merge::engine::Content::Named(identity)) => identity.hash,
      other => panic!("`b` not named: {other:?}"),
    };
    (hg.root, base)
  });
  let (submitted, _, _) = crate::test_store::run_on_first(
    &daemon,
    move || async move { edited_named(green).await },
    NAMED_PATIENCE,
  );
  assert!(
    !matches!(submitted, ReplyBody::Refused { .. }),
    "{submitted:?}"
  );
  crate::daemon::observe_first(&daemon, move |state| {
    crate::vcs_store::name_holds::let_go_landed_texts(state, &[base]);
  });
  let (brought, held_after) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      crate::vcs_store::hg_named::NOT_WHOLE.with(|refuse| refuse.set(true));
      let brought = crate::vcs_store::hg_named::bring_back_named_live(green).await;
      let held_after = crate::repository::svn_commit::tests::on(|state| {
        crate::vcs_store::name_holds::let_go_landed_texts(state, &brought);
        state
          .vcs_stores
          .content()
          .is_some_and(|content| content.holds_whole(&slates_vcs::ContentId(base)))
      });
      (brought, held_after)
    },
    NAMED_PATIENCE,
  );
  daemon.stop();
  assert!(
    brought.contains(&base),
    "the base returned for its let-go: {brought:?}"
  );
  assert!(!held_after, "the base let go again after the refused group");
}

/// Retires every member of `repository`'s lineage, the latest joined first, their engines and arenas released.
fn retire_lineage(state: &mut ShardState, repository: RepositoryId) {
  let members: Vec<DbVolumeId> = state
    .db
    .partition()
    .repositories()
    .members_of(repository)
    .map(|member| member.green)
    .collect();
  for member in members.into_iter().rev() {
    if let Some((lineage, Ok(mut release))) = state.greens.retire(&member) {
      while state
        .greens
        .release_step(lineage, &mut release)
        .is_ok_and(|more| more)
      {}
    }
  }
}

/// A green re-adopted at its boot's ledger, as a test leaves one (a restart's takeover, A-333): its engine gone and its
/// lineage slot released, awaiting its re-adoption through version 0.
fn awaiting_readoption(state: &mut ShardState, green: DbVolumeId) {
  if let Some((lineage, Ok(mut release))) = state.greens.retire(&green) {
    while state
      .greens
      .release_step(lineage, &mut release)
      .is_ok_and(|more| more)
    {}
  }
  state.greens.remove(&green);
  state.merge.awaiting_adoption.insert(
    green,
    crate::green_adopt::AwaitingAdoption {
      through: 0,
      identity: None,
      replay: None,
    },
  );
}

/// A re-adoption's branch member (main 01a20182's order of 2026-11-24 08:52, item (1), from w-hg's sweep for 259b0db84):
/// a branch re-adopted while its source stands in the lineage is rebuilt over that source, as a recovery rebuilds one
/// (`verbs::rebuild_member_sliced`), not installed alone over the replica's arenas. Do: on a store-backed daemon,
/// rebuild the fixtures' repository as one push (its root green, and bookmark `main`'s green branched from it); leave
/// `main` awaiting its re-adoption, its root in the lineage; re-adopt `main` over a fresh engine
/// (`green_adopt::readopt_with`); read `b` on it and edit `b` over a work on it. Expect: re-adopted, none installed
/// outside its lineage (`merge.readoption.outside_lineage` 0); `b` `b\n`; the edit landed (its changeset made over the
/// lineage's preview). Failing first: installed alone, counted outside its lineage, the edit refused.
#[test]
fn a_readopted_branch_member_is_rebuilt_over_its_source_and_takes_a_made_commit() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (hg, _) = rebuilt_on(&daemon, "branch-readopt", (&[1], 1));
  let branch = crate::daemon::observe_first(&daemon, move |state| {
    let branch = hg
      .bound(state, &bookmark_name(b"main"))
      .expect("`main`'s green");
    assert_ne!(branch, hg.root, "a branch of its own");
    awaiting_readoption(state, branch);
    branch
  });
  let (readopted, outside, read, edited) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let shard = crate::state::with_state(|state| state.shard).unwrap();
      let readopted =
        crate::green_adopt::readopt_with(shard, shard, branch, slates_merge::engine::Green::new())
          .await;
      let outside = crate::repository::svn_commit::tests::on(|state| {
        state
          .refusals
          .get(crate::verbs::READOPTED_OUTSIDE_LINEAGE)
          .copied()
          .unwrap_or(0)
      });
      if readopted != Some(true) {
        return (readopted, outside, None, None);
      }
      let read = crate::vcs_store::named_objects::read_file_brought(branch, "b").await;
      let (edited, _, _) = edited_named_as(branch, ("edit-branch", 1)).await;
      (readopted, outside, read, Some(edited))
    },
    NAMED_PATIENCE,
  );
  daemon.stop();
  assert_eq!(readopted, Some(true), "re-adopted");
  assert_eq!(outside, 0, "none installed outside its lineage");
  assert_eq!(read.as_deref(), Some(&b"b\n"[..]), "`b` on the branch");
  assert!(
    matches!(&edited, Some(reply) if !matches!(reply, ReplyBody::Refused { .. })),
    "{edited:?}"
  );
}

/// A boot's branch member whose root awaits its re-adoption (main 01a20182's order of 2026-11-24 08:52, item (2), from
/// w-hg's sweep for 259b0db84): it waits for its source rather than being fenced, and is rebuilt over it once the root is
/// re-adopted. Do: on a store-backed daemon, rebuild the fixtures' repository as one push (its root, and bookmark
/// `main`'s green branched from it); leave the root awaiting its re-adoption and `main` as a boot finds it, its engine
/// gone; rebuild `main` from its records as a boot does (`verbs::rebuild_member`); re-adopt the root
/// (`green_adopt::readopt_with`); read `b` on `main`. Expect: `main` not fenced by its rebuild; after the root's
/// re-adoption, `main` served, `b` `b\n`. Failing first: `main` fenced `SourceUnavailable` at its rebuild, its source not
/// yet standing, and never served.
#[test]
fn a_branch_whose_root_awaits_readoption_waits_and_is_rebuilt_over_it() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (hg, _) = rebuilt_on(&daemon, "branch-waits", (&[1], 1));
  let (root, branch, fenced) = crate::daemon::observe_first(&daemon, move |state| {
    let branch = hg
      .bound(state, &bookmark_name(b"main"))
      .expect("`main`'s green");
    // As a boot finds them: the branch's engine gone first, then the root's, which awaits its re-adoption.
    if let Some((lineage, Ok(mut release))) = state.greens.retire(&branch) {
      while state
        .greens
        .release_step(lineage, &mut release)
        .is_ok_and(|more| more)
      {}
    }
    state.greens.remove(&branch);
    awaiting_readoption(state, hg.root);
    let record = state.db.partition().volume(branch).cloned().unwrap();
    let _ = crate::verbs::rebuild_member(state, &record);
    let fenced = state.fenced_greens.get(&branch).cloned();
    (hg.root, branch, fenced)
  });
  let (readopted, read) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let shard = crate::state::with_state(|state| state.shard).unwrap();
      let readopted =
        crate::green_adopt::readopt_with(shard, shard, root, slates_merge::engine::Green::new())
          .await;
      let read = crate::vcs_store::named_objects::read_file_brought(branch, "b").await;
      (readopted, read)
    },
    NAMED_PATIENCE,
  );
  daemon.stop();
  assert_eq!(fenced, None, "not fenced while its source awaits");
  assert_eq!(readopted, Some(true), "the root re-adopted");
  assert_eq!(read.as_deref(), Some(&b"b\n"[..]), "`main` rebuilt over it");
}

/// A green taken over before its repository is rebuilt (main 01a20182's rulings of 2026-11-24 09:26 and 09:46, item
/// (2)'s second half, from w-hg's sweep for 259b0db84): between a takeover's adoption of a repository's green and the
/// repository's rebuild (`takeover::rebuild_repository`, which joins its members and makes its store), no membership names
/// it a repository's here, its record pending (`ShardState::pending_repositories`). Do: on a store-backed daemon,
/// rebuild the fixtures' repository as one push; leave it awaiting its rebuild, its store and texts gone as a successor
/// holds none (`test_store::awaiting_rebuild`); edit `b` over a work on its root and submit it as a client's recorded
/// request; then rebuild the repository (`test_store::rebuilt_after_takeover`) and send the same request again, as a
/// client resends one answered `Overloaded`. Expect: the first refused `Overloaded`, counted `merge.made.unbuilt`, the
/// root's head unmoved; the rebuild asked the node's changeset store for the texts its content store never knew
/// (`vcs_store.hg_replay_unknown_asked`, A-636 part 2c-2); the resend accepted, a changeset made of it (the root's
/// Mercurial head moved). Failing first: the first submit committed a version with no changeset made of it; then, its
/// store gone, the rebuild refused `Unheld` at the changelog's revision 0 and the resend `ContentUnavailable`.
#[test]
fn a_submit_over_a_green_whose_repository_awaits_its_rebuild_is_refused_until_it_is_rebuilt() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  // Its texts kept resident: the submit's own commit is what is judged here, not a read back of a let-go base.
  crate::test_store::keep_landed(&daemon);
  // The state logs' parts as the partition makes them, kept as they come, as a successor's acceptors hold them: what
  // its rebuild keeps (A-636 part 2c-2).
  crate::daemon::observe_first(&daemon, crate::state_log::keep_for_test);
  let (hg, _) = rebuilt_on(&daemon, "pending-rebuild", (&[1], 1));
  let (root, record, head, node) = crate::daemon::observe_first(&daemon, move |state| {
    let parts = crate::state_log::kept_for_test(state);
    let node = crate::vcs_store::hg_made::head_of(state, hg.root);
    // Its store gone too: a successor holds none, nor any of its texts, until its rebuild makes them from the log.
    let record = crate::test_store::awaiting_rebuild(state, hg.repository, true);
    (
      hg.root,
      (record, parts),
      state.db.partition().green_head(hg.root),
      node,
    )
  });
  let submit = slates_wire::request::RequestId {
    client: 7,
    sequence: 1,
  };
  let (first, unbuilt, after_first) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      use crate::repository::svn_commit::tests::on;
      let work = edited_work(root, "edit-pending").await;
      let first = on(|state| {
        crate::verbs::run_recorded_for_test(
          state,
          (7, submit),
          &owner(),
          crate::identities::authored(work, "edit a file"),
        )
      });
      let counted = on(|state| {
        state
          .refusals
          .get(crate::made_commit::MADE_UNBUILT)
          .copied()
          .unwrap_or(0)
      });
      let head = on(|state| state.db.partition().green_head(root));
      let repository = record.0.id;
      on(|state| crate::test_store::rebuilt_after_takeover(state, record));
      // Its store made from the log by the shard's own task, a step a turn: the resend comes once it stands, as a
      // client's does after its backoff.
      let deadline = slates_rt::futures::now_ns()
        .saturating_add(u64::try_from(NAMED_PATIENCE.as_nanos()).unwrap_or(u64::MAX));
      while on(|state| state.vcs_stores.rebuilding(repository)) {
        assert!(
          slates_rt::futures::now_ns() < deadline,
          "the store never stood"
        );
        slates_rt::futures::yield_now().await;
      }
      let unknown = on(|state| {
        state
          .refusals
          .get(crate::vcs_store::HG_REPLAY_UNKNOWN_ASKED)
          .copied()
          .unwrap_or(0)
      });
      let _ = on(|state| {
        crate::verbs::run_recorded_for_test(
          state,
          (7, submit),
          &owner(),
          crate::identities::authored(work, "edit a file"),
        )
      });
      let again = recorded_answer(7, submit).await;
      (first, counted, (head, again, unknown))
    },
    NAMED_PATIENCE,
  );
  let (moved, node_after) = crate::daemon::observe_first(&daemon, move |state| {
    (
      state.db.partition().green_head(root),
      crate::vcs_store::hg_made::head_of(state, root),
    )
  });
  daemon.stop();
  assert!(
    matches!(
      first,
      Some(ReplyBody::Refused {
        refusal: slates_ipc::protocol::Refusal::Overloaded { .. }
      })
    ),
    "the first refused Overloaded, retryable: {first:?}"
  );
  assert_eq!(unbuilt, 1, "counted merge.made.unbuilt");
  let (head_after_first, again, unknown) = after_first;
  assert_eq!(head_after_first, head, "nothing committed while it awaited");
  assert!(
    unknown > 0,
    "the rebuild asked for the texts its store never knew"
  );
  assert!(
    !matches!(again, ReplyBody::Refused { .. }),
    "the resend after the rebuild: {again:?}"
  );
  assert!(moved > head, "the resend committed");
  assert_ne!(node_after, node, "a changeset made of it");
}

/// A-636 part 2c-2's let-go (main 01a20182's ruling of 2026-11-24 13:12, w-hg's review note (c)): what a takeover's
/// successor's rebuild brings back from the node's changeset store goes once the rebuild lands, as a landed push's texts
/// go (goal 93, C1 (1) P5), through the same let-go. Do: on a store-backed daemon whose landings let go (no
/// `keep_landed`), rebuild the fixtures' repository as one push; leave it awaiting its rebuild, its store and texts gone
/// (`test_store::awaiting_rebuild`); rebuild it as its successor does (`test_store::rebuilt_after_takeover`) and wait for
/// the store to stand and its let-go to end. Expect: texts asked for identities the store never knew
/// (`vcs_store.hg_replay_unknown_asked`), as many revisions let go again (`vcs_store.hg_brought_back_let_go`), and none
/// of the rebuilt index's changelog, manifest or file objects held whole after (the empty window aside, never let go).
/// Failing first: every one the rebuild brought back held, nothing let go.
#[test]
fn a_successors_rebuild_lets_go_the_texts_it_brought_back_once_it_lands() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  crate::daemon::observe_first(&daemon, crate::state_log::keep_for_test);
  let (hg, _) = rebuilt_on(&daemon, "let-go-rebuild", (&[1], 1));
  let record = crate::daemon::observe_first(&daemon, move |state| {
    let parts = crate::state_log::kept_for_test(state);
    (
      crate::test_store::awaiting_rebuild(state, hg.repository, true),
      parts,
    )
  });
  let repository = record.0.id;
  let (unknown, let_go, held) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      use crate::repository::svn_commit::tests::on;
      let count =
        |name: &'static str| on(move |state| state.refusals.get(name).copied().unwrap_or(0));
      on(|state| crate::test_store::rebuilt_after_takeover(state, record));
      let deadline = slates_rt::futures::now_ns()
        .saturating_add(u64::try_from(NAMED_PATIENCE.as_nanos()).unwrap_or(u64::MAX));
      while on(|state| state.vcs_stores.rebuilding(repository))
        || count(crate::vcs_store::HG_BROUGHT_BACK_LET_GO) == 0
      {
        if slates_rt::futures::now_ns() >= deadline {
          break;
        }
        slates_rt::futures::yield_now().await;
      }
      let held = on(|state| held_whole(state, repository));
      (
        count(crate::vcs_store::HG_REPLAY_UNKNOWN_ASKED),
        count(crate::vcs_store::HG_BROUGHT_BACK_LET_GO),
        held,
      )
    },
    NAMED_PATIENCE,
  );
  daemon.stop();
  assert!(
    unknown > 0,
    "the rebuild asked for texts its store never knew"
  );
  assert_eq!(let_go, unknown, "every revision it brought back let go");
  assert_eq!(held, Vec::<(Vec<u8>, u32)>::new(), "nothing held whole");
}

/// Every replay lets go what it brought back (main 01a20182's ruling of 2026-11-24 13:22): a restart's, which asks the
/// node's changeset store for the texts its predecessor let go once landed (goal 93, C1 (1) P5), as a successor's asks
/// for those it never knew. Do: on a daemon over a segment and a store whose landings let go, rebuild the fixtures'
/// repository as one push and wait until its changelog's texts are let go, then publish the shard's image; stop the
/// daemon and the store's owner, start both again over the same segment and store, and wait for the recovery's let-go
/// to end. Expect: the replay asked for texts (`vcs_store.hg_replay_texts_asked` and `vcs_store.hg_replay_unknown_asked`), as many revisions let go again
/// (`vcs_store.hg_brought_back_let_go`), and none of the index's changelog, manifest or file objects held whole after.
/// Failing first: every text the replay brought back held, nothing let go.
#[test]
fn a_restarts_replay_lets_go_the_texts_it_brought_back() {
  use crate::repository::store_tests::{anchor_of, daemon_over};
  let instance = format!("vcs-store-hg-let-go-{}", std::process::id());
  let mut store = crate::test_store::TestStore::start("hg-let-go-restart");
  let anchor = anchor_of(&instance);
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  daemon.bootstrap(true).unwrap();
  let (hg, _) = rebuilt_on(&daemon, "let-go-restart", (&[1], 1));
  let repository = hg.repository;
  let landed_held = |daemon: &crate::Daemon| {
    crate::daemon::observe_first(daemon, move |state| {
      held_whole(state, repository)
        .into_iter()
        .filter(|(path, _)| path.as_slice() == slates_hg::push::intake::CHANGELOG_PATH)
        .count()
    })
  };
  let began = std::time::Instant::now();
  while landed_held(&daemon) > 0 {
    assert!(
      began.elapsed() < NAMED_PATIENCE,
      "the push's texts never let go"
    );
  }
  // The let-go carried by the shard's image, so the restart finds the texts away, as a period's publication leaves them.
  crate::daemon::observe_first(&daemon, |state| {
    crate::verbs::publish_shard(state).unwrap();
  });
  daemon.stop();
  store.restart_owner();
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let count = |daemon: &crate::Daemon, name: &'static str| {
    crate::daemon::observe_first(daemon, move |state| {
      state.refusals.get(name).copied().unwrap_or(0)
    })
  };
  let began = std::time::Instant::now();
  while count(&daemon, crate::vcs_store::HG_BROUGHT_BACK_LET_GO) == 0
    && began.elapsed() < NAMED_PATIENCE
  {}
  let asked = count(&daemon, crate::vcs_store::HG_REPLAY_TEXTS_ASKED)
    + count(&daemon, crate::vcs_store::HG_REPLAY_UNKNOWN_ASKED);
  let let_go = count(&daemon, crate::vcs_store::HG_BROUGHT_BACK_LET_GO);
  let held = crate::daemon::observe_first(&daemon, move |state| held_whole(state, repository));
  daemon.stop();
  assert!(asked > 0, "the replay asked for the texts let go");
  assert_eq!(let_go, asked, "every revision it brought back let go");
  assert_eq!(held, Vec::<(Vec<u8>, u32)>::new(), "nothing held whole");
}

/// Repository `repository`'s changelog, manifest and file objects its content store holds whole, the empty window aside
/// (never let go): each by its log's path and number.
fn held_whole(state: &mut ShardState, repository: RepositoryId) -> Vec<(Vec<u8>, u32)> {
  use slates_hg::push::intake::{CHANGELOG_PATH, MANIFEST_PATH};
  let lent = crate::vcs_store::Lent::new(&mut state.store, &mut state.vcs_stores);
  let (content, store) = lent.stores.mercurial(repository).expect("the store");
  let content: &slates_vcs::ContentStore = content;
  let index = &store.index;
  let whole = |id: &slates_vcs::ContentId| {
    content.len_of(id).is_some_and(|len| len > 0) && content.holds_whole(id)
  };
  let mut held = Vec::new();
  for rev in 0..index.len(content, Log::Changelog) {
    if index
      .changeset(content, rev)
      .is_some_and(|row| whole(&row.text))
    {
      held.push((CHANGELOG_PATH.to_vec(), rev));
    }
  }
  let paths: Vec<Vec<u8>> = index.file_logs(content).map(<[u8]>::to_vec).collect();
  let logs = std::iter::once((MANIFEST_PATH.to_vec(), Log::Manifest))
    .chain(paths.iter().map(|path| (path.clone(), Log::File(path))));
  for (path, log) in logs {
    for rev in 0..index.len(content, log) {
      let row = index.revision(content, log, rev);
      if row.is_some_and(|row| whole(&row.bytes) || row.block.as_ref().is_some_and(whole)) {
        held.push((path.clone(), rev));
      }
    }
  }
  held
}

/// A work over green `green` named `name`, `x` inserted at offset 1 of `b`: the work.
async fn edited_work(green: DbVolumeId, name: &str) -> slates_ipc::protocol::VolumeId {
  use crate::repository::svn_commit::tests::on;
  let made = on(|state| {
    crate::vcs_store::paths::tests::ask(
      state,
      RequestBody::CreateWork {
        green: slates_ipc::protocol::VolumeId { bytes: green.bytes },
        name: name.to_owned(),
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
  work
}

/// A daemon over `instance`'s segment and `store`, the anchor's, with a Mercurial repository rebuilt from the fixtures by
/// one push to its empty first line: the daemon and the repository's root green.
fn named_restart_daemon(
  instance: &str,
  anchor: &(slates_anchor::AnchorSegment, slates_ipc::HeldRendezvous),
  store: &crate::test_store::TestStore,
) -> (crate::Daemon, DbVolumeId) {
  let daemon = crate::repository::store_tests::daemon_over(instance, (&anchor.0, &anchor.1), store);
  daemon.bootstrap(true).unwrap();
  let (hg, _) = rebuilt_on(&daemon, "named-restart", (&[1], 1));
  (daemon, hg.root)
}

/// A-923 (main's rulings of 2026-11-25: an image written before the aways carried their let-go numbers is migrated at
/// its restore, since a refused away loses a Mercurial named file, brought back by its bytes). Do: on a daemon over a
/// segment and a store, rebuild the fixtures' repository as one push, its root green holding `b` by its object's name and
/// that object let go; write the image's Aways family in the form before A-923 and publish; stop the daemon and the
/// store's owner, start both again over the same segment and store; read every file the green names through its name.
/// Expect: aways written in the old form, every one taken back numbered (`vcs_store::AWAYS_RENUMBERED`), and every
/// named file read back as the push held it, `b` `b\n`. Red with the old form refused: `b` unread, its windows absent.
#[test]
fn a_restart_on_an_image_written_before_a923_reads_every_named_pushed_file() {
  let instance = format!("vcs-store-hg-a923-{}", std::process::id());
  let mut store = crate::test_store::TestStore::start("hg-a923");
  let anchor = crate::repository::store_tests::anchor_of(&instance);
  let (daemon, green) = named_restart_daemon(&instance, &anchor, &store);
  let (old, named) = crate::daemon::observe_first(&daemon, move |state| {
    let named: Vec<String> = state
      .greens
      .get(&green)
      .map(|engine| {
        engine
          .files()
          .filter(|(_, content)| matches!(content, slates_merge::engine::Content::Named(_)))
          .map(|(path, _)| path)
          .collect()
      })
      .unwrap_or_default();
    (crate::content_holder::write_aways_before_a923(state), named)
  });
  daemon.publish_every_shard().unwrap();
  daemon.stop();
  store.restart_owner();
  let daemon =
    crate::repository::store_tests::daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let renumbered = crate::daemon::observe_first(&daemon, |state| {
    state
      .refusals
      .get(crate::vcs_store::AWAYS_RENUMBERED)
      .copied()
      .unwrap_or(0)
  });
  let paths = named.clone();
  let files = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let mut files = Vec::new();
      for path in &paths {
        files.push(crate::vcs_store::named_objects::read_file_brought(green, path).await);
      }
      files
    },
    NAMED_PATIENCE,
  );
  daemon.stop();
  println!("a923 hg: {old} aways in the old form, {renumbered} renumbered, named {named:?}");
  assert!(old > 0, "the image held windows let go");
  assert!(named.iter().any(|path| path == "b"), "`b` named: {named:?}");
  for (path, file) in named.iter().zip(&files) {
    assert!(file.is_some(), "`{path}` read back through its name");
  }
  let b = named
    .iter()
    .position(|path| path == "b")
    .and_then(|at| files[at].clone());
  assert_eq!(b.as_deref(), Some(&b"b\n"[..]), "`b` as the push held it");
  assert_eq!(renumbered, old as u64, "every old away taken back numbered");
}

/// A-887 (main 01a20182's ruling of 2026-11-24 02:17, A-760's replay: a run its apply names whole is never read). Do: on
/// a daemon over a segment and a store, rebuild the fixtures' repository as one push, its root green holding `b` by its
/// object's name and that object let go; stop the daemon and the store's owner, start both again over the same segment
/// and store; then read `b` through its name. Expect: the green recovered at the push's version, unfenced, its chain
/// replayed with no text brought back for it (`merge.name_read.brought_back` 0 before the read); `b` then `b\n`.
/// Failing first: the green fenced `ContentMissing { version: 1 }`, the replay reading the push's named runs from the
/// content store before applying its entry, their windows let go.
#[test]
fn a_restart_replays_a_push_naming_its_files_without_reading_their_texts() {
  let instance = format!("vcs-store-hg-replay-{}", std::process::id());
  let mut store = crate::test_store::TestStore::start("hg-named-replay");
  let anchor = crate::repository::store_tests::anchor_of(&instance);
  let (daemon, green) = named_restart_daemon(&instance, &anchor, &store);
  let version =
    crate::daemon::observe_first(&daemon, move |state| state.db.partition().green_head(green));
  daemon.stop();
  store.restart_owner();
  let daemon =
    crate::repository::store_tests::daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let recovered = crate::daemon::observe_first(&daemon, move |state| {
    let head = state.greens.get(&green).map(|engine| engine.head());
    let brought = state
      .refusals
      .get(crate::vcs_store::named_objects::READ_BROUGHT_BACK)
      .copied()
      .unwrap_or(0);
    (head, state.fenced_greens.get(&green).cloned(), brought)
  });
  let file = crate::test_store::run_on_first(
    &daemon,
    move || async move { crate::vcs_store::named_objects::read_file_brought(green, "b").await },
    NAMED_PATIENCE,
  );
  daemon.stop();
  assert_eq!(
    recovered,
    (Some(version), None, 0),
    "recovered at the push's version, unfenced, nothing brought back"
  );
  assert_eq!(file.as_deref(), Some(&b"b\n"[..]), "`b` read by its name");
}

/// A-887 (main 01a20182's rulings (B) of 2026-11-24 00:16 and (A) of 03:00: a restart's recovery, in dependency order).
/// Do: on a daemon over a segment and a store, rebuild the fixtures' repository as one push, its root green holding `b`
/// by its object's name and that object let go, then edit `b` in place over a work and submit it (`edited_named`), its
/// base let go again; stop
/// the daemon and the store's owner, start both again over the same segment and store; read `b` through the work, as a
/// client's recorded request, then through the green's name. Expect: the submit landed before the restart; after it, the
/// green rebuilt after the repositories' stores, with the bookmark `main`'s green branched from it
/// (`merge.recovery.greens_after_stores` 2), its spliced base brought back
/// by the revision its chain's changeset names (`vcs.hg.named_recovery_brought_back` 1), recovered at the submit's
/// version, unfenced; the work, which waited for its green, served `bx\n`, never refused; the green's `b` `bx\n`.
/// Failing first: the green fenced `ContentMissing { version: 2 }`, the splice's base unread: the greens were rebuilt
/// before the stores, so no index resolved it.
#[test]
fn a_restart_recovers_a_green_whose_named_pushed_file_a_work_edited() {
  use crate::repository::store_tests::{anchor_of, daemon_over};
  let instance = format!("vcs-store-hg-named-{}", std::process::id());
  let mut store = crate::test_store::TestStore::start("hg-named-restart");
  let anchor = anchor_of(&instance);
  let (daemon, green) = named_restart_daemon(&instance, &anchor, &store);
  let base = crate::daemon::observe_first(&daemon, move |state| {
    let named = state
      .greens
      .get(&green)
      .and_then(|engine| match engine.content("b")? {
        slates_merge::engine::Content::Named(identity) => Some(identity.hash),
        slates_merge::engine::Content::Bytes(_) => None,
      });
    named.expect("`b` named by the push")
  });
  let (submitted, _, work) = crate::test_store::run_on_first(
    &daemon,
    move || async move { edited_named(green).await },
    NAMED_PATIENCE,
  );
  // The base the submit brought back let go again, as a landed text's windows go: the restart must bring it back.
  let away = crate::daemon::observe_first(&daemon, move |state| {
    crate::vcs_store::name_holds::let_go_landed_texts(state, &[base]);
    state
      .vcs_stores
      .content()
      .is_some_and(|content| !content.holds_whole(&slates_vcs::ContentId(base)))
  });
  assert!(away, "the base let go again");
  // Published with its base away, so the restart's image holds it away.
  daemon.publish_every_shard().unwrap();
  assert!(
    !matches!(submitted, ReplyBody::Refused { .. }),
    "{submitted:?}"
  );
  let version =
    crate::daemon::observe_first(&daemon, move |state| state.db.partition().green_head(green));
  daemon.stop();
  store.restart_owner();
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let recovered = crate::daemon::observe_first(&daemon, move |state| {
    let head = state.greens.get(&green).map(|engine| engine.head());
    let counted = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
    (
      head,
      state.fenced_greens.get(&green).cloned(),
      counted("merge.recovery.greens_after_stores"),
      counted(crate::vcs_store::hg_named::RECOVERY_BROUGHT_BACK),
    )
  });
  let (through_work, file) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let read = slates_wire::request::RequestId {
        client: 7,
        sequence: 2,
      };
      let _ = crate::repository::svn_commit::tests::on(|state| {
        crate::verbs::run_recorded_for_test(
          state,
          (7, read),
          &owner(),
          RequestBody::ReadRange {
            volume: work,
            path: "b".to_owned(),
            at: slates_ipc::protocol::ReadAt::Head,
            offset: 0,
            max: 64,
          },
        )
      });
      let through_work = recorded_answer(7, read).await;
      let file = crate::vcs_store::named_objects::read_file_brought(green, "b").await;
      (through_work, file)
    },
    NAMED_PATIENCE,
  );
  daemon.stop();
  assert_eq!(
    recovered,
    (Some(version), None, 2, 1),
    "rebuilt after the stores, its base brought back, recovered at the submit's version, unfenced"
  );
  let ReplyBody::ReadPage { bytes, .. } = through_work else {
    panic!("the work's read was not served a page: {through_work:?}");
  };
  assert_eq!(bytes, b"bx\n", "the work, served after its green");
  assert_eq!(
    file.as_deref(),
    Some(&b"bx\n"[..]),
    "the edited file after the restart"
  );
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
  let (hg, _) = rebuilt_on(&daemon, "named-edit", (&[1], 1));
  let (green, named_away) = crate::daemon::observe_first(&daemon, move |state| {
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
      let (submitted, parked, _) = edited_named(green).await;
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
  let made = history();
  let at = |name: &[u8]| BookmarkAt {
    name: name.to_vec(),
    node: Some(made[1].node),
  };
  let unbundle = Unbundle {
    parts: vec![
      taken(0, Asks::Changegroup(changegroup(&made, &[0, 1]))),
      taken(
        1,
        Asks::Bookmarks(vec![at(b"main"), at(b"left"), at(b"right")]),
      ),
    ],
  };
  let hg = made_and_pushed_on(&daemon, "named-twice", unbundle);
  let greens = crate::daemon::observe_first(&daemon, move |state| {
    let greens = [b"main".as_slice(), b"left", b"right"].map(|name| {
      hg.bound(state, &bookmark_name(name))
        .expect("a bookmark's green")
    });
    let named = greens.map(|green| {
      matches!(
        state.greens.get(&green).and_then(|engine| engine
          .content("b")
          .map(|held| { matches!(held, slates_merge::engine::Content::Named(_)) })),
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
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = Hg::made_non_publishing("plain").await;
    let made = rebuilt(&hg, (&[1], 1)).await;
    assert_rebuilt_awaited(&hg, &made).await;
    crate::repository::svn_commit::tests::on(|state| {
      let main = hg.bound(state, &bookmark_name(b"main")).unwrap();
      assert_eq!(head(state, main), Some(made[1].node));
      assert!(hg.roots(state).is_empty());
    });
    let answer = pushed_bytes(&hg, fixture!("push-plain.hg")).await;
    let Outcome::Accepted(reply) = answer else {
      panic!("not accepted: {answer:?}");
    };
    assert_eq!(encoded(&reply), fixture!("push-plain-reply.hg").to_vec());
    let version = crate::repository::svn_commit::tests::on(|state| {
      assert_plain_landed(state, &hg, &made);
      state.db.partition().green_head(hg.root)
    });
    let read = crate::vcs_store::named_objects::read_file_brought(hg.root, "d").await;
    assert_eq!(
      read.as_deref(),
      Some(&b"d\n"[..]),
      "the pushed file read through its name"
    );
    let mut unbundle = stock(fixture!("push-plain.hg"));
    unbundle
      .parts
      .retain(|part| !matches!(part.asks, Asks::CheckUpdatedHeads(_) | Asks::CheckPhases(_)));
    let again = pushed(&hg, &unbundle).await;
    let Outcome::Accepted(reply) = again else {
      panic!("not accepted again: {again:?}");
    };
    assert_eq!(encoded(&reply), fixture!("push-plain-reply.hg").to_vec());
    assert_eq!(
      crate::repository::svn_commit::tests::on(|state| state.db.partition().green_head(hg.root)),
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
  let (hg, made) = rebuilt_on(&daemon, "replayed", (&[1], 1));
  pushed_on(&daemon, hg, fixture!("push-plain.hg"));
  let (repository, made, before) = crate::daemon::observe_first(&daemon, move |state| {
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
  crate::daemon::observe_first(&daemon, |_| {
    super::super::hg_run::TEST_SEGMENT_BYTES.with(|bound| bound.set(SEGMENT_BYTES));
  });
  let unbundle = Unbundle {
    parts: vec![taken(0, Asks::Changegroup(part))],
  };
  let hg = made_and_pushed_on(&daemon, "pieces", unbundle);
  let repository = crate::daemon::observe_first(&daemon, move |state| {
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
  assert_eq!(hg.roots(state), BTreeSet::from([made[3].node]));
  assert!(hg.holds(state, &made[3].node));
}

/// A-657 part 4 (`HG_TXNID`; 7.2's `localrepo.transaction`). Do: take the transaction ids of a push's name, of the same
/// name again and of another. Expect: `TXN:` and forty lower-case hexadecimal digits; the same name gives the same id,
/// another name another. (A prepared push's id is its name's by construction, `prepare`; no store is needed to hold it.)
#[test]
fn a_push_names_its_transaction_as_7_2_does() {
  let transaction = transaction_id(&NAME);
  let digits = transaction.strip_prefix("TXN:").unwrap();
  assert_eq!(digits.len(), 40);
  assert!(
    digits
      .bytes()
      .all(|digit| matches!(digit, b'0'..=b'9' | b'a'..=b'f'))
  );
  assert_eq!(transaction_id(&NAME), transaction_id(&[7; 32]));
  assert_ne!(transaction_id(&NAME), transaction_id(&[8; 32]));
}

/// A-657, A-642 (a race found where the entry appends). Do: on the rebuilt repository, prepare stock `push-plain`
/// (its check that `second` is a head holds), then land another client's push moving `default` (`other`), then land
/// the prepared push. Expect: the other push lands; the prepared one is refused as 7.2's race, byte for byte its
/// `push-race` reply, and nothing of it is applied: `default` at `other`, its changeset not held, no root of it.
#[test]
fn a_push_whose_line_moved_after_its_read_is_answered_as_7_2s_race() {
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = Hg::made_non_publishing("race").await;
    let made = rebuilt(&hg, (&[1], 1)).await;
    // The stock push's changegroup is taken into its fork and its segments landed, and it holds at its landing; then
    // another push lands; then the stock push lands as the driver lands it, its stale fork re-based by awaiting the
    // store and the race judged at landing (A-720 part 2b).
    super::hold_next_landing();
    let stock_push = spawned_push(hg, fixture!("push-plain.hg").to_vec());
    until_held().await;
    let other = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[2])))],
    };
    let landed = pushed(&hg, &other).await;
    assert!(matches!(landed, Outcome::Accepted(_)), "{landed:?}");
    assert_eq!(
      crate::repository::svn_commit::tests::on(|state| head(state, hg.root)),
      Some(made[2].node)
    );
    super::release_held_landing();
    let race = match answered(&stock_push).await {
      Outcome::Refused(reply) => Err(encoded(&reply)),
      other => Ok(format!("{other:?}")),
    };
    assert_eq!(race, Err(fixture!("push-race-reply.hg").to_vec()));
    crate::repository::svn_commit::tests::on(|state| {
      assert_eq!(head(state, hg.root), Some(made[2].node));
      assert!(!hg.holds(state, &made[3].node));
      assert_eq!(hg.roots(state), BTreeSet::from([made[2].node]));
    });
  });
}

/// A-720 part 2b (pushes taken side by side). Do: on the rebuilt repository, take two pushes' changegroups into their
/// own forks from the same index (`other` and `plain`, each a child of `second`, neither asking a race check), land the
/// first, then land the second, whose fork the first made stale. Expect both accepted and both changesets held by the
/// repository's index after: the second re-based by node, nothing of the first lost.
#[test]
fn two_pushes_taken_side_by_side_both_land() {
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = Hg::made_non_publishing("side-by-side").await;
    let made = rebuilt(&hg, (&[1], 1)).await;
    let first = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[2])))],
    };
    let second = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[3])))],
    };
    // The second's changegroup taken into its fork from the index as it is, then held at its landing; the first lands;
    // the second, its fork made stale, re-based by node and landed.
    super::hold_next_landing();
    let held = spawned_push(hg, bundled(&second));
    until_held().await;
    let landed = pushed(&hg, &first).await;
    assert!(matches!(landed, Outcome::Accepted(_)), "{landed:?}");
    super::release_held_landing();
    let landed = answered(&held).await;
    assert!(matches!(landed, Outcome::Accepted(_)), "{landed:?}");
    crate::repository::svn_commit::tests::on(|state| {
      assert!(hg.holds(state, &made[2].node) && hg.holds(state, &made[3].node))
    });
  });
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
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = Hg::made_non_publishing("heads").await;
    let made = rebuilt(&hg, (&[1], 1)).await;
    let first = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[4])))],
    };
    let landed = pushed(&hg, &first).await;
    assert!(matches!(landed, Outcome::Accepted(_)), "{landed:?}");
    let stable = crate::repository::svn_commit::tests::on(|state| {
      let stable = hg.bound(state, &line_name(b"stable", 0)).unwrap();
      assert_eq!(head(state, stable), Some(made[4].node));
      stable
    });
    let checked = Unbundle {
      parts: vec![
        taken(0, Asks::CheckHeads(vec![made[4].node])),
        taken(1, Asks::Changegroup(changegroup(&made, &[3]))),
      ],
    };
    // Its changegroup taken into its fork and held at its landing (A-720); another push moves `stable`; then it lands
    // as the driver lands it, its stale fork re-based and `check:heads` judged at landing.
    super::hold_next_landing();
    let held = spawned_push(hg, bundled(&checked));
    until_held().await;
    let again = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[5])))],
    };
    let moved = pushed(&hg, &again).await;
    assert!(matches!(moved, Outcome::Accepted(_)), "{moved:?}");
    assert_eq!(
      crate::repository::svn_commit::tests::on(|state| head(state, stable)),
      Some(made[5].node)
    );
    super::release_held_landing();
    let raced = answered(&held).await;
    let words = super::RACED.to_vec();
    assert_eq!(
      raced,
      Outcome::refused(&Refused::PushRaced { message: words })
    );
    crate::repository::svn_commit::tests::on(|state| {
      assert_eq!(head(state, hg.root), Some(made[1].node));
      assert!(!hg.holds(state, &made[3].node));
    });
  });
}

/// A-657 (§4.17.3 decision 3: phases moved in the same entry). Do: rebuild with only `base` published and `main` at
/// `second`, then land stock `push-phase` (its `phase-heads` part publishes `second`). Expect: before it, `second` the
/// one draft root; it is answered byte for byte as 7.2 answered it (a stream with no part), the root removed, the
/// phases log one position on.
#[test]
fn a_published_draft_leaves_the_roots_in_the_same_entry() {
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = Hg::made_non_publishing("phase").await;
    let made = rebuilt(&hg, (&[0], 1)).await;
    let position = crate::repository::svn_commit::tests::on(|state| {
      assert_eq!(hg.roots(state), BTreeSet::from([made[1].node]));
      hg.phases(state)
    });
    let answer = pushed_bytes(&hg, fixture!("push-phase.hg")).await;
    let Outcome::Accepted(reply) = answer else {
      panic!("not accepted: {answer:?}");
    };
    assert_eq!(encoded(&reply), fixture!("push-phase-reply.hg").to_vec());
    crate::repository::svn_commit::tests::on(|state| {
      assert!(hg.roots(state).is_empty());
      assert_eq!(hg.phases(state), position + 1);
    });
  });
}

/// A-657 (§4.17.3 decision 2: a bookmark is a green). Do: rebuild with only `base` published and `main` at `base`,
/// then land stock `push-bookmark` (`check:bookmarks` and a `bookmarks` part moving `main` to `second`). Expect:
/// before it, `main` a green at `base` without `b`; it is answered byte for byte as 7.2 answered it, and `main`'s green
/// moved to `second`, holding `b`; `default`'s line untouched.
#[test]
fn a_bookmark_moves_on_its_own_green() {
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = Hg::made_non_publishing("bookmark").await;
    let made = rebuilt(&hg, (&[0], 0)).await;
    let (main, version) = crate::repository::svn_commit::tests::on(|state| {
      let main = hg.bound(state, &bookmark_name(b"main")).unwrap();
      assert_eq!(head(state, main), Some(made[0].node));
      assert_eq!(file(state, main, "b"), None);
      (main, state.db.partition().green_head(hg.root))
    });
    let answer = pushed_bytes(&hg, fixture!("push-bookmark.hg")).await;
    let Outcome::Accepted(reply) = answer else {
      panic!("not accepted: {answer:?}");
    };
    assert_eq!(encoded(&reply), fixture!("push-bookmark-reply.hg").to_vec());
    assert_eq!(
      crate::repository::svn_commit::tests::on(|state| head(state, main)),
      Some(made[1].node)
    );
    let read = crate::vcs_store::named_objects::read_file_brought(main, "b").await;
    assert_eq!(read.as_deref(), Some(&b"b\n"[..]));
    assert_eq!(
      crate::repository::svn_commit::tests::on(|state| state.db.partition().green_head(hg.root)),
      version
    );
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

/// Sets repository `name`'s policy to `rules`, from version `read`, not publishing (as the tests' pushes were judged).
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
      hg_publishing: false,
    },
  );
  assert_eq!(reply, ReplyBody::Acknowledged);
}

/// Configures repository `name`'s policy, from its version 1 ([`Hg::made_non_publishing`]'s), to run the
/// `pretxnchangegroup` hook `hook`, not publishing, its
/// program the test binary, a path absolute on the test's host, as the guard asks (A-622).
fn set_hook(state: &mut ShardState, name: &str, hook: &str) {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::SetRepositoryPolicy {
      repository: name.to_owned(),
      read: 1,
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
      hg_publishing: false,
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
  crate::test_store::run_on_stored_shard(|| async move {
    let driven = Hg::made_non_publishing("driven").await;
    let made = rebuilt(&driven, (&[1], 1)).await;
    let hooked = Hg::made_non_publishing("hooked").await;
    let _ = rebuilt(&hooked, (&[1], 1)).await;
    let version = crate::repository::svn_commit::tests::on(|state| {
      set_hook(state, "hooked", "pretxnchangegroup.check");
      state.db.partition().green_head(hooked.root)
    });
    let refused = pushed_bytes(&hooked, fixture!("push-plain.hg")).await;
    let message = b"the repository's pretxnchangegroup.check hook cannot run here yet: its view of the push is \
                    not served"
      .to_vec();
    assert_eq!(
      refused,
      Outcome::refused(&Refused::Abort {
        message,
        hint: None
      })
    );
    crate::repository::svn_commit::tests::on(|state| {
      assert_eq!(state.db.partition().green_head(hooked.root), version);
      assert!(!hooked.holds(state, &made[3].node));
    });
    let accepted = pushed_bytes(&driven, fixture!("push-plain.hg")).await;
    let Outcome::Accepted(reply) = accepted else {
      panic!("not accepted: {accepted:?}");
    };
    assert_eq!(encoded(&reply), fixture!("push-plain-reply.hg").to_vec());
    crate::repository::svn_commit::tests::on(|state| {
      assert_eq!(head(state, driven.root), Some(made[3].node));
      assert!(driven.holds(state, &made[3].node));
    });
  });
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
  crate::test_store::run_on_stored_shard(|| async move {
    let plain = Hg::made_non_publishing("acl-plain").await;
    let _ = rebuilt(&plain, (&[1], 1)).await;
    crate::repository::svn_commit::tests::on(|state| {
      set_policy(
        state,
        ("acl-plain", 1),
        vec![rule(
          PolicyNames::Branches,
          "default",
          (PolicyEffect::Deny, PolicyWho::Anyone),
        )],
      );
    });
    let denied = format!(
      "acl: user \"{:?}\" denied on branch \"default\" (changeset \"048741fabe42\")",
      owner()
    );
    assert_eq!(
      pushed_bytes(&plain, fixture!("push-plain.hg")).await,
      Outcome::refused(&Refused::Abort {
        message: denied.into_bytes(),
        hint: None
      })
    );
    crate::repository::svn_commit::tests::on(|state| {
      set_policy(state, ("acl-plain", 2), Vec::new())
    });
    let admitted = pushed_bytes(&plain, fixture!("push-plain.hg")).await;
    assert!(matches!(admitted, Outcome::Accepted(_)), "{admitted:?}");
    let marks = Hg::made_non_publishing("acl-mark").await;
    let _ = rebuilt(&marks, (&[0], 0)).await;
    let only = PolicyWho::AnyOf {
      entries: vec![PolicyEntry::Is {
        principal: stranger(),
      }],
    };
    crate::repository::svn_commit::tests::on(|state| {
      set_policy(
        state,
        ("acl-mark", 1),
        vec![rule(
          PolicyNames::Bookmarks,
          "*",
          (PolicyEffect::Allow, only),
        )],
      );
    });
    let second = "59d5dc23f8786c06d0fae53e625be1cdd640a783";
    let listed = format!(
      "acl: user \"{:?}\" not allowed on bookmark \"main\" (changeset \"{second}\")",
      owner()
    );
    assert_eq!(
      pushed_bytes(&marks, fixture!("push-bookmark.hg")).await,
      Outcome::refused(&Refused::Abort {
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
  crate::test_store::run_on_stored_shard(|| async move {
    let marks = Hg::made_non_publishing("acl-delete").await;
    let _ = rebuilt(&marks, (&[1], 1)).await;
    crate::repository::svn_commit::tests::on(|state| {
      set_policy(
        state,
        ("acl-delete", 1),
        vec![PolicyRule {
          names: PolicyNames::Bookmarks,
          pattern: "*".to_owned(),
          right: PolicyRight::Delete,
          effect: PolicyEffect::Deny,
          who: PolicyWho::Anyone,
        }],
      );
    });
    let deleting = Unbundle {
      parts: vec![taken(
        0,
        Asks::Bookmarks(vec![BookmarkAt {
          name: b"main".to_vec(),
          node: None,
        }]),
      )],
    };
    let denied = format!(
      "acl: user \"{:?}\" denied on bookmark \"main\" (changeset \"\")",
      owner()
    );
    assert_eq!(
      pushed(&marks, &deleting).await,
      Outcome::refused(&Refused::Abort {
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
  crate::test_store::run_on_stored_shard(|| async move {
    let two = Hg::made_non_publishing("acl-order").await;
    let made = rebuilt(&two, (&[1], 1)).await;
    crate::repository::svn_commit::tests::on(|state| {
      set_policy(
        state,
        ("acl-order", 1),
        vec![rule(
          PolicyNames::Branches,
          "*",
          (PolicyEffect::Deny, PolicyWho::Anyone),
        )],
      );
    });
    let pushing = Unbundle {
      parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[4, 3])))],
    };
    let stable = hex(&made[4].node);
    let denied = format!(
      "acl: user \"{:?}\" denied on branch \"stable\" (changeset \"{}\")",
      owner(),
      stable.get(..12).unwrap()
    );
    assert_eq!(
      pushed(&two, &pushing).await,
      Outcome::refused(&Refused::Abort {
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
        let _spawner = crate::spawning::TestSpawner::installed();
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
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = Hg::made_non_publishing("marks").await;
    let made = rebuilt(&hg, (&[1], 1)).await;
    let at = |name: &[u8]| BookmarkAt {
      name: name.to_vec(),
      node: Some(made[1].node),
    };
    let unbundle = Unbundle {
      parts: vec![taken(0, Asks::Bookmarks(vec![at(b"left"), at(b"right")]))],
    };
    let answer = pushed(&hg, &unbundle).await;
    assert!(matches!(answer, Outcome::Accepted(_)), "{answer:?}");
    crate::repository::svn_commit::tests::on(|state| {
      let bound = |name: &[u8]| hg.bound(state, &bookmark_name(name));
      let (main, left, right) = (bound(b"main"), bound(b"left"), bound(b"right"));
      let greens: BTreeSet<DbVolumeId> = [main, left, right].into_iter().flatten().collect();
      assert_eq!(greens.len(), 3, "{main:?} {left:?} {right:?}");
      for green in [left, right].into_iter().flatten() {
        assert_eq!(head(state, green), Some(made[1].node));
      }
    });
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
    let entry = slates_hg::import::record::SegmentEntry {
      sequence,
      record: ([0; 32], 0),
      payload: ([0; 32], 0),
      texts: ([0; 32], 0),
    };
    run.landed(entry, &[]);
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
  let hg = made_on(&daemon, "dropped");
  let (repository, root, named) = crate::daemon::observe_first(&daemon, move |state| {
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
  let abandoned = crate::daemon::observe_first(&daemon, |state| {
    counted(state, super::super::hg_run::RUNS_ABANDONED)
  });
  let made = history_of(&[
    ("one", "one", b"one\n", None, "default"),
    ("two", "two", b"two\n", Some(0), "default"),
  ]);
  let unbundle = Unbundle {
    parts: vec![taken(0, Asks::Changegroup(changegroup(&made, &[0, 1])))],
  };
  pushed_unbundle_on(&daemon, hg, unbundle);
  // The push's entry names its segment list (record v12, ruling R), read back from the node's store by its identity.
  let first = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let (homed, disk) = crate::repository::svn_commit::tests::on(|state| {
        let records = state.db.partition().repositories();
        let homed = (0..)
          .map_while(|index| records.object_record(repository, index))
          .filter_map(slates_hg::import::record::homed_run_of)
          .last();
        (homed, state.vcs_disk.clone())
      });
      let (homed, disk) = (homed?, disk?);
      let list = crate::vcs_store::keyed_bytes(&disk, homed.list, homed.len).await?;
      slates_hg::import::record::segment_list_of(&list)?
        .first()
        .map(|entry| entry.sequence)
    },
    NAMED_PATIENCE,
  );
  let tip = crate::daemon::observe_first(&daemon, move |state| {
    crate::vcs_store::rebuild(state, repository);
    made[1].node
  });
  let rebuilt = until(&daemon, move |state| {
    !state.vcs_stores.rebuilding(repository)
  });
  let head = crate::daemon::observe_first(&daemon, move |state| head(state, root));
  daemon.stop();
  assert!(landed, "the task's run landed a changeset at a sequence");
  assert!(forgotten, "the dropped run's rows forgotten");
  assert_eq!(abandoned, 1);
  assert_eq!(
    first,
    Some(named + 2),
    "the push's entry names its run, past the abandoned sequence"
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
  crate::test_store::run_on_stored_shard(|| async move {
    crate::repository::svn_commit::tests::on(|state| {
      crate::vcs_share::wait::TEST_CEILING.with(|ceiling| ceiling.set(1_000));
      let fits = super::need_of(state, (100, 400, 500), 2_000);
      let passes = super::need_of(state, (100, 400, 501), 2_000);
      crate::vcs_share::wait::TEST_CEILING.with(|ceiling| ceiling.set(0));
      assert_eq!(fits.ok(), Some(1_000));
      assert_eq!(passes.err(), Some(super::past_ceiling()));
    });
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

/// Shape: the lines the moved-heads test pushes and then moves, each a line green of its own: enough that their works in
/// one landing step show (about 12.5 ms a green's work, debug, 2026-11-24), few enough that the red's one step stays
/// inside a debug build's watchdog window.
const MOVED_HEADS: usize = 32;

/// The moved-heads history: a root and [`MOVED_HEADS`] - 1 changesets on it, each a head of its own (so the push makes a
/// line green for each), then a child on each of those heads, which moves its line: the history, the first push's
/// revisions, and the second's.
fn moved_heads_history() -> (Vec<Made>, Vec<usize>, Vec<usize>) {
  let names: &'static [String] = Box::leak(
    (0..MOVED_HEADS)
      .map(|at| format!("head-{at}"))
      .chain((1..MOVED_HEADS).map(|at| format!("moved-{at}")))
      .collect::<Vec<_>>()
      .into_boxed_slice(),
  );
  let commits: Vec<Commit> = names
    .iter()
    .enumerate()
    .map(|(at, name)| {
      let parent = match at {
        0 => None,
        at if at < MOVED_HEADS => Some(0),
        at => Some(at + 1 - MOVED_HEADS),
      };
      (
        name.as_str(),
        name.as_str(),
        name.as_bytes(),
        parent,
        "default",
      )
    })
    .collect();
  (
    history_of(&commits),
    (0..MOVED_HEADS).collect(),
    (MOVED_HEADS..commits.len()).collect(),
  )
}

/// Goal 93, the landing spin's owed (1) (main's order of 2026-11-26: a push moving hundreds of existing heads did their
/// works in its landing's one step, since an update's green is a reader's and is not made ahead). Do: push a root and
/// [`MOVED_HEADS`] - 1 heads on it, each a line green; then push a child on each of those heads, which moves every line,
/// through the daemon's driver (`hg_push::unbundle`), the history's polls timed by the wall and by the thread's CPU.
/// Expect both pushes accepted; neither landing step doing a line's work itself (`repository.push.landing_step_works_most`
/// 0), each moved line's work judged before the step a slice at a time (`repository.push.update_slices` moved) and taken
/// by it (`repository.push.update_misses` 0). The shard's and the history's longest poll printed, for main's release
/// probe. Red before: the second push's step did 31 lines' works (`~/.local/state/w-hg-logs/2026-11-26-moved-heads-red.txt`).
#[test]
fn t_goal93_a_push_moving_many_heads_judges_their_works_ahead_of_its_landing_step() {
  let (made, first, second) = moved_heads_history();
  let first = crate::vcs_http::daemon_edge_tests::history_bundle(&made, &first);
  let second = crate::vcs_http::daemon_edge_tests::history_bundle(&made, &second);
  let seen = crate::test_store::run_on_stored_shard(move || {
    crate::vcs_store::svn_import::tests::cpu_timed(async move {
      let hg = Hg::made("moved-heads").await;
      let made = pushed_bytes_to(hg.repository, &first).await;
      let before = crate::repository::svn_commit::tests::on(|state| {
        let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
        count(crate::repository::push::LANDING_STEP_WORKS)
      });
      let moved = pushed_bytes_to(hg.repository, &second).await;
      let counted = crate::repository::svn_commit::tests::on(|state| {
        let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
        (
          count(crate::repository::push::LANDING_STEP_WORKS),
          count(crate::repository::push::UPDATE_SLICES),
          count(crate::repository::push::UPDATE_MISSES),
          count(crate::repository::push::LANDING_STEP_NS),
        )
      });
      let (longest_ns, polls) = crate::vcs_store::svn_import::tests::longest_poll();
      (made, moved, before, counted, longest_ns, polls)
    })
  });
  let (made, moved, before, (in_step, slices, misses, step_ns), longest_ns, polls) = seen;
  eprintln!(
    "moved heads: the landing step {} us at most, {in_step} works in it; the shard's longest poll {} us ({polls})",
    step_ns / 1000,
    longest_ns / 1000
  );
  assert!(matches!(made, Outcome::Accepted(_)), "{made:?}");
  assert!(matches!(moved, Outcome::Accepted(_)), "{moved:?}");
  assert_eq!(
    (before, in_step),
    (0, 0),
    "neither push's step did a line's work itself (the first's root line moved by its work judged ahead too)"
  );
  assert!(
    slices > 0,
    "the moved lines' works judged a slice at a time: {slices}"
  );
  assert_eq!(
    misses, 0,
    "every work judged ahead taken by the landing's step"
  );
}

/// Shape: the files a wide line's whole tree holds: enough that a whole tree a line shows in the landing's record.
const WIDE_TREE: usize = 200;
/// Shape: the wide-lines push's heads, each a line of its own past the first.
const WIDE_HEADS: usize = 32;
/// Shape: the bytes of versions the wide-lines push's landing record carried while each new line's work was built
/// eagerly (w-hg's probe of `apply_planned`, 2026-11-26, debug: 33 versions in 329,007 bytes, about 10 KB a line for a
/// one-file change): the red, which the owed pushes must stay far under.
const WIDE_EAGER_VERSIONS_BYTES: u64 = 329_007;

/// A history of `tree` changesets on `default`, each adding one file to the one before, then `heads` changesets on the
/// last, each adding a file of its own, each a head: the history and its every revision.
fn wide_history(tree: usize, heads: usize) -> (Vec<Made>, Vec<usize>) {
  let names: &'static [String] = Box::leak(
    (0..tree)
      .map(|at| format!("file-{at}"))
      .chain((0..heads).map(|at| format!("head-{at}")))
      .collect::<Vec<_>>()
      .into_boxed_slice(),
  );
  let commits: Vec<Commit> = names
    .iter()
    .enumerate()
    .map(|(at, name)| {
      let parent = match at {
        0 => None,
        at if at < tree => Some(at - 1),
        _ => Some(tree - 1),
      };
      (
        name.as_str(),
        name.as_str(),
        name.as_bytes(),
        parent,
        "default",
      )
    })
    .collect();
  let made = history_of(&commits);
  let all = (0..made.len()).collect();
  (made, all)
}

/// The lines of `repository` that owe their pushes (P2), each a new line a push made.
fn owing_lines(state: &ShardState, repository: RepositoryId) -> Vec<DbVolumeId> {
  state
    .db
    .partition()
    .repositories()
    .members_of(repository)
    .map(|member| member.green)
    .filter(|green| state.greens.is_owing(green))
    .collect()
}

/// A file as a green holds it: its path, and its length and hash, whether its bytes are held or named (A-887).
type HeldFile = (String, (u64, [u8; 32]));

/// What `green` reads, materialized: its head, its files (each as [`HeldFile`]) and its head's identity.
fn read_whole(state: &mut ShardState, green: DbVolumeId) -> (u64, Vec<HeldFile>, [u8; 32]) {
  let mut engine = state.greens.get_mut(&green).expect("materialized");
  let files = engine
    .files()
    .map(|(path, content)| (path, content.digest()))
    .collect();
  (engine.head(), files, engine.head_identity())
}

/// Goal 93, the landing spin's (2) and (3), closed by P2 for Mercurial (main's ruling (A) of 2026-11-26). Do: push a
/// history of [`WIDE_TREE`] changesets, each adding a file, and [`WIDE_HEADS`] heads on it, each a line; then read one
/// new line through the door (`green_ready::made_for`), the history's polls timed. Expect: the push accepted; every new
/// line owing its push, made in the landing's step with no work there (`repository.push.landing_step_works_most` 0,
/// `landing_step_creates_most` the new lines); the record's versions far under the eager build's
/// [`WIDE_EAGER_VERSIONS_BYTES`] (a tenth), since each new line's is its owed push's small record, not its tree; and the
/// line read made, its first read's wait counted (`merge.green.materialize_wait_ns_most`) and printed with the
/// shard's longest poll, for main's release probe.
#[test]
fn t_goal93_a_push_of_many_new_lines_records_each_as_its_owed_push_not_its_tree() {
  let (made, all) = wide_history(WIDE_TREE, WIDE_HEADS);
  let bundle = crate::vcs_http::daemon_edge_tests::history_bundle(&made, &all);
  let seen = crate::test_store::run_on_stored_shard(move || {
    crate::vcs_store::svn_import::tests::cpu_timed(async move {
      let hg = Hg::made("wide-lines").await;
      let pushed = pushed_bytes_to(hg.repository, &bundle).await;
      let (owing, counted) = crate::repository::svn_commit::tests::on(|state| {
        let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
        (
          owing_lines(state, hg.repository),
          (
            count(crate::repository::push::LANDING_STEP_WORKS),
            count(crate::repository::push::LANDING_STEP_CREATES),
            count(crate::repository::push::ENTRY_VERSIONS_BYTES_MOST),
          ),
        )
      });
      let mut pinned = crate::green_ready::Pinned::default();
      let made = match owing.first() {
        Some(green) => crate::green_ready::made_for(*green, &mut pinned).await,
        None => None,
      };
      let waited = crate::repository::svn_commit::tests::on(|state| {
        let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
        count(crate::green_ready::MATERIALIZE_WAIT_NS_MOST)
      });
      drop(pinned);
      let (longest_ns, polls) = crate::vcs_store::svn_import::tests::longest_poll();
      (
        pushed,
        owing.len(),
        counted,
        made,
        waited,
        longest_ns,
        polls,
      )
    })
  });
  let (pushed, owing, (works, creates, bytes), made, waited, longest_ns, polls) = seen;
  eprintln!(
    "wide lines: {owing} owing, {creates} made in the step, {works} works in it, {bytes} bytes of versions in its record; \
     a line's first read waited {} us for its build; the shard's longest poll {} us ({polls})",
    waited / 1000,
    longest_ns / 1000
  );
  assert!(matches!(pushed, Outcome::Accepted(_)), "{pushed:?}");
  assert_eq!(owing, WIDE_HEADS - 1, "every new line owes its push");
  assert_eq!(works, 0, "the landing's step did no line's work");
  assert_eq!(
    creates,
    u64::try_from(owing).unwrap(),
    "each new line made in the step, owing"
  );
  assert!(
    bytes < WIDE_EAGER_VERSIONS_BYTES / 10,
    "the record's versions no longer carry each line's tree: {bytes} bytes"
  );
  assert_eq!(made, Some(true), "the line read was made");
  assert!(waited > 0, "its first read's wait counted");
}

/// P2 for Mercurial's oracle (main's ruling (A) of 2026-11-26, requirement 3). Do: on a daemon over a segment and a
/// store, push a root and three heads on it to one repository, each head past the first a new line owing its push, and
/// to another the root and the first new line's head alone, which that repository's `default` moves to eagerly. Stop
/// the daemon and the store's owner and start both again, so the owed pushes come back by the recovery's replay of
/// their records (`slates_merge::push::DeferredPush`); then read the owed line naming that head through the door.
/// Expect: the line still owing after the restart, then made, and reading as the eager line: the same files and the
/// same head identity (its own head counts its versions from its own base, the line's version 0).
#[test]
fn an_owed_line_made_after_a_restart_reads_as_the_eager_line() {
  use crate::repository::store_tests::{anchor_of, daemon_over};
  let commits: &'static [Commit] = &[
    ("root", "a", b"alpha", None, "default"),
    ("one", "b", b"beta", Some(0), "default"),
    ("two", "c", b"gamma", Some(0), "default"),
    ("three", "d", b"delta", Some(0), "default"),
  ];
  let made = history_of(commits);
  let all = crate::vcs_http::daemon_edge_tests::history_bundle(&made, &[0, 1, 2, 3]);
  let instance = format!("hg-owed-restart-{}", std::process::id());
  let mut store = crate::test_store::TestStore::start("hg-owed-restart");
  let anchor = anchor_of(&instance);
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  daemon.bootstrap(true).unwrap();
  let owed = made_on(&daemon, "owed-lines");
  pushed_bundle_on(&daemon, &owed, all);
  let owing: Vec<(DbVolumeId, Option<Node>)> =
    crate::daemon::observe_first(&daemon, move |state| {
      owing_lines(state, owed.repository)
        .into_iter()
        .map(|green| (green, crate::vcs_store::hg_made::head_of(state, green)))
        .collect()
    });
  assert_eq!(
    owing.len(),
    3 - 1,
    "each head past the first a line owing its push"
  );
  // The line the eager repository is to hold: one owed line, by the head its push sets.
  let (line, head) = owing.first().copied().expect("an owed line");
  let head = head.expect("its owed push names its head");
  let at = made
    .iter()
    .position(|made| made.node == head)
    .expect("a head of the history");
  let eager = made_on(&daemon, "eager-line");
  let alone = crate::vcs_http::daemon_edge_tests::history_bundle(&made, &[0, at]);
  pushed_bundle_on(&daemon, &eager, alone);
  crate::daemon::observe_first(&daemon, |state| {
    crate::verbs::publish_shard(state).unwrap();
  });
  daemon.stop();
  store.restart_owner();
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let read = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let still = crate::repository::svn_commit::tests::on(|state| state.greens.is_owing(&line));
      let mut pinned = crate::green_ready::Pinned::default();
      let made = crate::green_ready::made_for(line, &mut pinned).await;
      let reads = crate::repository::svn_commit::tests::on(|state| {
        let owed = read_whole(state, line);
        let eager = read_whole(state, eager.root);
        (owed, eager)
      });
      (still, made, reads)
    },
    NAMED_PATIENCE,
  );
  daemon.stop();
  let (still, made, (owed, eager)) = read;
  assert!(still, "the line owes its push again after the restart");
  assert_eq!(made, Some(true), "made through the door");
  let paths: Vec<&str> = owed.1.iter().map(|(path, _)| path.as_str()).collect();
  assert_eq!(paths.len(), 2, "the root's file and the head's: {paths:?}");
  assert_eq!(owed.1, eager.1, "the same files");
  assert_eq!(owed.2, eager.2, "the same head identity");
}

/// P2 for Mercurial: a build dropped mid-way (main's ruling (A) of 2026-11-26, requirement 3; A-857's wait for room).
/// Do: push a root and two heads, the second a new line owing its push; read that line through the door with its first
/// build given a room of one byte, so the build is dropped past its room mid-way and begun again within the room that
/// comes. Expect: the line made, once (`merge.green.materialized` 1), after one wait (`merge.green.materialize_waited`
/// 1); its chain still the owed push's record alone (the dropped build named nothing); and it reads the files its head
/// checks out.
#[test]
fn an_owed_lines_build_dropped_mid_way_names_nothing_and_is_made_again() {
  let commits: &'static [Commit] = &[
    ("root", "a", b"alpha", None, "default"),
    ("one", "b", b"beta", Some(0), "default"),
    ("two", "c", b"gamma", Some(0), "default"),
  ];
  let made = history_of(commits);
  let bundle = crate::vcs_http::daemon_edge_tests::history_bundle(&made, &[0, 1, 2]);
  let seen = crate::test_store::run_on_stored_shard(move || async move {
    let hg = Hg::made("dropped-line").await;
    let pushed = pushed_bytes_to(hg.repository, &bundle).await;
    let line = crate::repository::svn_commit::tests::on(|state| owing_lines(state, hg.repository))
      .first()
      .copied();
    crate::green_ready::TEST_ROOMS.with(|rooms| rooms.borrow_mut().push_back(1));
    let mut pinned = crate::green_ready::Pinned::default();
    let made = match line {
      Some(green) => crate::green_ready::made_for(green, &mut pinned).await,
      None => None,
    };
    let after = crate::repository::svn_commit::tests::on(|state| {
      let files = line.map(|green| read_whole(state, green).1);
      let chain = line.map_or(0, |green| state.db.partition().green_chain(green).len());
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      (
        count(crate::green_ready::MATERIALIZED),
        count(crate::green_ready::MATERIALIZE_WAITED),
        chain,
        files,
      )
    });
    (pushed, made, after)
  });
  let (pushed, made, (materialized, waited, chain, files)) = seen;
  assert!(matches!(pushed, Outcome::Accepted(_)), "{pushed:?}");
  assert_eq!(made, Some(true), "the line made");
  assert_eq!((materialized, waited), (1, 1), "made once, after one wait");
  assert_eq!(chain, 1, "its chain the owed push's record alone");
  let files: Vec<String> = files
    .unwrap_or_default()
    .into_iter()
    .map(|(path, _)| path)
    .collect();
  assert!(
    files.len() == 2
      && files
        .iter()
        .all(|path| path == "a" || path == "b" || path == "c"),
    "{files:?}"
  );
}

/// A-929's replica half (main's order of 2026-11-26: P2 for Mercurial on a fleet). Do: push a root and two heads, the
/// second a new line owing its push; make a holder's replica of that line as a holder's records make it (its source's
/// replica at the version it was made from, the owner's shipped versions applied; the line branched there; the owner's shipped
/// version 1, the line's owed push, applied, `merge_service::apply_inputs`), so the replica owes the push (the line made
/// from `default`'s empty root, a holder's replica of which is an empty green; a base laid out is the owner's tests' same
/// lend); begin its
/// build as a holder does (`replica_build::begin`) and wait for it; then make the owner's line through the door. Expect:
/// the replica made (`merge.replica_build.made` 1), none refused (`merge.replica_build.refused` 0), and its tree the
/// owner's: the same files and head identity. Red before: refused, since a holder lent no Mercurial layout.
#[test]
fn a_holders_replica_of_an_owed_line_is_made_and_reads_as_the_owners_line() {
  let commits: &'static [Commit] = &[
    ("root", "a", b"alpha", None, "default"),
    ("one", "b", b"beta", Some(0), "default"),
    ("two", "c", b"gamma", Some(0), "default"),
  ];
  let made = history_of(commits);
  let bundle = crate::vcs_http::daemon_edge_tests::history_bundle(&made, &[0, 1, 2]);
  let seen = crate::test_store::run_on_stored_shard(move || async move {
    use crate::repository::svn_commit::tests::on;
    let hg = Hg::made("replica-line").await;
    let pushed = pushed_bytes_to(hg.repository, &bundle).await;
    let line = on(|state| owing_lines(state, hg.repository))
      .first()
      .copied()
      .expect("an owed line");
    let key = crate::merge_service::replica_key(slates_db::register::ObjectId(line.bytes));
    let owes = on(|state| {
      let Some(slates_db::catalog::MemberOrigin::Branch { from, version }) = state
        .db
        .partition()
        .repositories()
        .member(line)
        .map(|member| member.origin)
      else {
        return Err("no branch origin".to_owned());
      };
      // A push into the empty repository makes its new lines from `default` at version 0, its empty root: the line's
      // version 0 is that root, so its replica there is a new green, as git's replica test makes one.
      if version != 0 {
        return Err(format!(
          "the line made at {from:?}'s version {version}, not its root"
        ));
      }
      state
        .merge
        .replicas
        .put_back_any(&key, slates_merge::engine::Green::new());
      let Some((inputs, _)) = crate::merge_service::shipped_for_test(state, line, 1) else {
        return Err("no shipped record".to_owned());
      };
      let Some(mut replica) = state.merge.replicas.take_any(&key) else {
        return Err("no replica".to_owned());
      };
      let applied = crate::merge_service::apply_inputs(&mut replica, &inputs, 1);
      let owes = !replica.is_materialized();
      state.merge.replicas.put_back_any(&key, replica);
      match (applied, owes) {
        (Ok(()), true) => Ok(()),
        other => Err(format!("applied, owing: {other:?}")),
      }
    });
    if let Err(why) = &owes {
      return (
        pushed,
        Err(why.clone()),
        (0, 0),
        None,
        (None, (Vec::new(), [0; 32])),
      );
    }
    on(|state| {
      crate::replica_build::begin(
        state,
        slates_db::register::ObjectId(line.bytes),
        hg.repository,
      )
    });
    let deadline = std::time::Instant::now() + NAMED_PATIENCE;
    let counted = loop {
      let counted = on(|state| {
        let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
        (
          count(crate::replica_build::MADE),
          count(crate::replica_build::REFUSED),
        )
      });
      if counted != (0, 0) || std::time::Instant::now() > deadline {
        break counted;
      }
      slates_rt::futures::yield_now().await;
    };
    let mut pinned = crate::green_ready::Pinned::default();
    let owner_made = crate::green_ready::made_for(line, &mut pinned).await;
    let reads = on(|state| {
      let replica = state.merge.replicas.take_any(&key).map(|mut replica| {
        let files: Vec<HeldFile> = replica
          .files()
          .map(|(path, content)| (path, content.digest()))
          .collect();
        let read = (files, replica.head_identity());
        state.merge.replicas.put_back_any(&key, replica);
        read
      });
      let owner = read_whole(state, line);
      (replica, (owner.1, owner.2))
    });
    (pushed, owes, counted, owner_made, reads)
  });
  let (pushed, owes, (made, refused), owner_made, (replica, owner)) = seen;
  assert!(matches!(pushed, Outcome::Accepted(_)), "{pushed:?}");
  assert_eq!(owes, Ok(()), "the holder's replica owes the line's push");
  assert_eq!((made, refused), (1, 0), "the replica made, none refused");
  assert_eq!(owner_made, Some(true), "the owner's line made");
  let replica = replica.expect("the replica");
  assert_eq!(
    replica.0.len(),
    2,
    "the root's file and the head's: {:?}",
    replica.0
  );
  assert_eq!(replica, owner, "the replica's tree is the owner's");
}
