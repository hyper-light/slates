//! Tests of the changesets the server makes (A-731): a submitted version made a changeset, a client's push on top of it
//! the head after, a second submit made over the push, and a recovery making the same again from the log.

use slates_db::catalog::{Principal, RepositoryId, VolumeId as DbVolumeId};
use slates_hg::index::Log;
use slates_hg::manifest::Flag;
use slates_hg::push::held::IndexHeld;
use slates_hg::push::made::{Authored, Change, made};
use slates_hg::push::{Asks, Taken, Unbundle};
use slates_hg::{Admission, NULL_NODE, Node};
use slates_ipc::protocol::{ObjectFormat, ReplyBody, RequestBody, VcsTool};

use super::{MADE, head_of};
use crate::repository::hg_push::tests::{Made, changegroup};
use crate::repository::hg_push::{land_in, line_name, take_whole};
use crate::state::ShardState;
use crate::vcs_store::paths::tests::{create, version};
use crate::verbs::to_db_volume;

/// Shape: how long a test waits for a shard's task (a rebuild) to end before it fails, as the import tests wait.
const TASK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// A Mercurial repository `name`, its first line `default`'s: its id and the line's green.
fn mercurial(state: &mut ShardState, name: &str) -> (RepositoryId, DbVolumeId) {
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
  (repository, to_db_volume(id))
}

/// The first parent of changeset `node` in the repository's index.
fn parent_of(state: &ShardState, repository: RepositoryId, node: &Node) -> Option<Node> {
  let (content, store) = state.vcs_stores.mercurial_ref(repository)?;
  let rev = store.index.rev(content, Log::Changelog, node)?;
  let row = store.index.changeset(content, rev)?;
  match row.p1 {
    slates_hg::index::NONE => Some(NULL_NODE),
    p1 => store.index.changeset(content, p1).map(|row| row.node),
  }
}

/// A client's push of a changeset adding `pushed.txt` over `parent`: its node, landed as the driver lands it.
fn push_on(state: &mut ShardState, repository: RepositoryId, parent: Node) -> Node {
  let verified = crate::vcs_store::lent_state(state, |state| {
    let (content, store) = state.vcs_stores.mercurial_ref(repository).unwrap();
    let authored = Authored {
      user: b"Client <c@example.com>",
      seconds: 1,
      branch: b"default",
      message: b"pushed",
    };
    let change = [Change {
      path: b"pushed.txt",
      after: Some((b"pushed\n", Flag::Regular)),
      copied_from: None,
    }];
    made(
      &IndexHeld::new(&store.index, content),
      parent,
      &change,
      &authored,
      &mut Admission::new(u64::MAX),
    )
    .unwrap()
  });
  let (changeset, manifest) = (&verified.changesets[0], &verified.manifests[0]);
  let (path, files) = &verified.files[0];
  let one = Made {
    node: changeset.node,
    p1: changeset.p1,
    text: changeset.text.clone(),
    manifest: (manifest.node, manifest.p1, manifest.text.clone()),
    file: (path.clone(), files[0].node, files[0].text.clone()),
  };
  let unbundle = Unbundle {
    parts: vec![Taken {
      id: 0,
      mandatory: true,
      asks: Asks::Changegroup(changegroup(&[one], &[0])),
    }],
  };
  let (_, part) = unbundle.changegroup().unwrap();
  let forked = take_whole(
    state,
    repository,
    (part.version, part.payload.clone()),
    &mut Admission::new(u64::MAX),
  )
  .unwrap()
  .unwrap();
  let landed = land_in(
    state,
    repository,
    (&owner(), b"pushed"),
    &unbundle,
    Some(forked),
    &mut Admission::new(u64::MAX),
  );
  assert!(
    matches!(landed, Some(slates_hg::push::Outcome::Accepted(_))),
    "{landed:?}"
  );
  changeset.node
}

/// A-731, A-767 C4. Do: on a running daemon holding a changeset store, make a Mercurial repository; submit a work
/// making `a.txt` to its line's green; push a client's changeset on top of the made one (landed on the store); submit
/// another work changing `a.txt`; then make the repository's store again from its log as a recovery does (the sliced
/// `vcs_store::rebuild`), and read it once the task ends. Expect: the first submit made a changeset over the null node,
/// the line's head; the push's changeset the head after it, its parent the made one; the second submit's changeset over
/// the push's, the head; and after the rebuild, the same head and every changeset held: the made nodes put back from
/// their records, the push's from its changeset on the store.
#[test]
fn t_a731_a_submitted_version_is_the_changeset_the_server_makes() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (repository, green, held) = crate::daemon::observe_first(&daemon, |state| {
    let (repository, green) = mercurial(state, "made");
    let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
    assert_eq!(
      version(state, ipc_green, 1, vec![create("a.txt", b"alpha\n")]),
      1
    );
    let first = head_of(state, green).expect("the submit made a changeset");
    assert_eq!(parent_of(state, repository, &first), Some(NULL_NODE));
    let pushed = push_on(state, repository, first);
    assert_eq!(
      head_of(state, green),
      Some(pushed),
      "the push on top is the head"
    );
    assert_eq!(parent_of(state, repository, &pushed), Some(first));
    assert_eq!(
      version(state, ipc_green, 2, vec![create("a.txt", b"beta\n")]),
      3
    );
    let second = head_of(state, green).expect("the second submit made a changeset");
    assert_eq!(parent_of(state, repository, &second), Some(pushed));
    assert_eq!(state.refusals.get(MADE).copied(), Some(2));
    crate::vcs_store::rebuild(state, repository);
    (repository, green, [first, pushed, second])
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
  let (head, parents) = crate::daemon::observe_first(&daemon, move |state| {
    (
      head_of(state, green),
      held.map(|node| parent_of(state, repository, &node).is_some()),
    )
  });
  daemon.stop();
  assert!(!rebuilding, "still rebuilding after {TASK_WAIT:?}");
  assert_eq!(head, Some(held[2]), "the made node from the record");
  assert_eq!(parents, [true; 3], "every changeset held after the rebuild");
}

/// A-731 with A-626 part 4 (a copy made through a work). Do: make a Mercurial repository; submit `a.txt`; then submit a
/// work declaring a copy of `/a.txt` to `/b.txt`, with no other change. Expect the second changeset to list `b.txt`,
/// its file revision's block naming `a.txt` and `a.txt`'s node in the first changeset's manifest (`copy`, `copyrev`),
/// its first parent null and its bytes `a.txt`'s, as stock 7.2 commits `hg cp a.txt b.txt`.
#[test]
fn t_a731_a_copy_a_work_declares_is_a_copy_in_the_made_changeset() {
  use crate::vcs_store::paths::tests::Op;
  use slates_ipc::protocol::WorkOp;
  crate::test_store::audit_on_stored_shard("hg", |state| {
    let (repository, green) = mercurial(state, "copied");
    let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
    version(state, ipc_green, 1, vec![create("a.txt", b"alpha\n")]);
    let copy = Op::Declare(WorkOp::Copy {
      from: "/a.txt".to_owned(),
      to: "/b.txt".to_owned(),
      version: None,
      replace: false,
    });
    assert_eq!(version(state, ipc_green, 2, vec![copy]), 2);
    // Read as every step reads the store: its windows lent from the shard's chunk store (A-727 a2).
    let (source, row, text) = crate::vcs_store::lent_state(state, |state| {
      let (content, store) = state
        .vcs_stores
        .mercurial_ref(repository)
        .expect("the store");
      let index = &store.index;
      let source = index
        .revision(content, Log::File(b"a.txt"), 0)
        .expect("a.txt's revision")
        .node;
      let row = index
        .revision(content, Log::File(b"b.txt"), 0)
        .expect("b.txt's revision");
      let text = index
        .revision_text(content, Log::File(b"b.txt"), 0)
        .expect("its text");
      (source, row, text)
    });
    let mut expected = b"\x01\ncopy: a.txt\ncopyrev: ".to_vec();
    expected.extend_from_slice(crate::verbs::hex_of(&source).as_bytes());
    expected.extend_from_slice(b"\n\x01\nalpha\n");
    assert_eq!(
      (String::from_utf8_lossy(&text).into_owned(), row.p1),
      (
        String::from_utf8_lossy(&expected).into_owned(),
        slates_hg::index::NONE
      )
    );
  });
}

/// A work over `green` writing `bytes` at `path`, made by bare dispatches: its id.
fn hg_work(
  state: &mut ShardState,
  green: DbVolumeId,
  (name, path, bytes): (&str, &str, &[u8]),
) -> slates_ipc::protocol::VolumeId {
  let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
  let made = crate::vcs_store::paths::tests::ask(
    state,
    RequestBody::CreateWork {
      green: ipc_green,
      name: name.to_owned(),
    },
  );
  let ReplyBody::WorkCreated { id: work, .. } = made else {
    panic!("no work: {made:?}")
  };
  let edited = crate::vcs_store::paths::tests::ask(
    state,
    RequestBody::Edit {
      work,
      path: path.to_owned(),
      at: 0,
      delete_len: 0,
      bytes: bytes.to_vec(),
    },
  );
  assert!(!matches!(edited, ReplyBody::Refused { .. }), "{edited:?}");
  work
}

/// What a client and a reader see of a Mercurial repository's green after a recorded submit: whether the request is
/// answered, the green's durable head, its engine's head, and the line's head changeset.
fn seen(
  state: &ShardState,
  green: DbVolumeId,
  id: slates_wire::request::RequestId,
) -> (bool, u64, Option<u64>, Option<Node>) {
  (
    matches!(
      state.db.partition().completion(7, id.client, id.sequence),
      slates_wire::request::Seen::Completed(_)
    ),
    state.db.partition().green_head(green),
    state.greens.get(&green).map(|engine| engine.head()),
    head_of(state, green),
  )
}

/// Polls `daemon`'s first shard until the request `id` is answered, within [`TASK_WAIT`]: what it saw then.
fn answered(
  daemon: &crate::daemon::Daemon,
  green: DbVolumeId,
  id: slates_wire::request::RequestId,
) -> (bool, u64, Option<u64>, Option<Node>) {
  let began = std::time::Instant::now();
  loop {
    let now = crate::daemon::observe_first(daemon, move |state| seen(state, green, id));
    if now.0 || began.elapsed() > TASK_WAIT {
      return now;
    }
  }
}

/// A-806 (main 02:37, piece 2: Mercurial's made changesets adopt the deferred-submit hook, `crate::made_commit`). Do:
/// submit a work to a Mercurial repository's green as a client's recorded request, and read it in the submit's own
/// step; then wait for the answer. Expect: in the submit's step nothing answered, nothing durable, the green's engine at
/// its old head and no made changeset; then the request answered with the green's durable and engine heads at 1 and the
/// made changeset the line's head, all from one step. Until the hook, the submit's own step committed and answered.
#[test]
fn a_mercurial_made_changeset_is_answered_only_after_its_one_step() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let id = slates_wire::request::RequestId {
    client: 7,
    sequence: 1,
  };
  let (green, in_the_step) = crate::daemon::observe_first(&daemon, move |state| {
    let (_, green) = mercurial(state, "deferred");
    let work = hg_work(state, green, ("deferred-work", "a.txt", b"alpha\n"));
    crate::verbs::run_recorded_for_test(
      state,
      (7, id),
      &owner(),
      crate::identities::authored(work, "made"),
    );
    (green, seen(state, green, id))
  });
  let after = answered(&daemon, green, id);
  daemon.stop();
  assert_eq!(
    in_the_step,
    (false, 0, Some(0), None),
    "nothing answered or shown in the submit's step"
  );
  assert_eq!(
    (after.0, after.1, after.2),
    (true, 1, Some(1)),
    "answered after the one step"
  );
  assert!(after.3.is_some(), "the made changeset is the line's head");
}

/// A-806 (the turn first). Do: hold a Mercurial repository's landing turn, submit a work to its green as a client's
/// recorded request, read it, then give the turn back and wait for the answer. Expect: while the turn is held nothing of
/// the submit ran (engine and durable heads 0, unanswered); once given back the parked submit runs again holding the
/// turn and is answered with the version and its made changeset. Until the hook a Mercurial submit took no turn.
#[test]
fn a_mercurial_submit_meeting_its_repositorys_turn_held_waits_and_commits_once_it_is_given_back() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let id = slates_wire::request::RequestId {
    client: 7,
    sequence: 1,
  };
  let (green, while_held) = crate::daemon::observe_first(&daemon, move |state| {
    let (repository, green) = mercurial(state, "turn-held");
    let work = hg_work(state, green, ("turn-work", "a.txt", b"alpha\n"));
    let held = crate::repository::svn_commit::landing::Turn::take_now(state, repository);
    crate::verbs::run_recorded_for_test(
      state,
      (7, id),
      &owner(),
      crate::identities::authored(work, "made"),
    );
    let seen_then = seen(state, green, id);
    if let Some(turn) = held {
      turn.give_back(state);
    }
    (green, seen_then)
  });
  let after = answered(&daemon, green, id);
  daemon.stop();
  assert_eq!(
    while_held,
    (false, 0, Some(0), None),
    "nothing ran while the turn was held"
  );
  assert_eq!(
    (after.0, after.1, after.2),
    (true, 1, Some(1)),
    "run again holding the turn"
  );
  assert!(after.3.is_some(), "the made changeset is the line's head");
}
