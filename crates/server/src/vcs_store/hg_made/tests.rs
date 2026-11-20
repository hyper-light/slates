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
use crate::verbs::{dispatch, to_db_volume};

fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// A Mercurial repository `name`, its first line `default`'s: its id and the line's green.
fn mercurial(state: &mut ShardState, name: &str) -> (RepositoryId, DbVolumeId) {
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

/// A-731. Do: make a Mercurial repository; submit a work making `a.txt` to its line's green; push a client's changeset
/// on top of the made one; submit another work changing `a.txt`; then make the repository's store again from its log as
/// a recovery does. Expect: the first submit made a changeset over the null node, the line's head; the push's changeset
/// the head after it, its parent the made one; the second submit's changeset over the push's, the head; and after the
/// rebuild, the same head and every changeset held: the made node put back from the record.
#[test]
fn t_a731_a_submitted_version_is_the_changeset_the_server_makes() {
  crate::daemon::audit_on_shard(|state| {
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
    assert_rebuilt_from_the_log(state, (repository, green), [first, pushed, second]);
  });
}

/// `repository`'s store made again from its log: `green`'s head still the last changeset of `held`, the made node
/// put back from the record, and every changeset of `held` held.
fn assert_rebuilt_from_the_log(
  state: &mut ShardState,
  (repository, green): (RepositoryId, DbVolumeId),
  held: [Node; 3],
) {
  crate::vcs_store::rebuild_whole(state, repository);
  assert_eq!(
    head_of(state, green),
    Some(held[2]),
    "the made node from the record"
  );
  for node in held {
    assert!(
      parent_of(state, repository, &node).is_some(),
      "{node:?} not held after the rebuild"
    );
  }
}

/// A-731 with A-626 part 4 (a copy made through a work). Do: make a Mercurial repository; submit `a.txt`; then submit a
/// work declaring a copy of `/a.txt` to `/b.txt`, with no other change. Expect the second changeset to list `b.txt`,
/// its file revision's block naming `a.txt` and `a.txt`'s node in the first changeset's manifest (`copy`, `copyrev`),
/// its first parent null and its bytes `a.txt`'s, as stock 7.2 commits `hg cp a.txt b.txt`.
#[test]
fn t_a731_a_copy_a_work_declares_is_a_copy_in_the_made_changeset() {
  use crate::vcs_store::paths::tests::Op;
  use slates_ipc::protocol::WorkOp;
  crate::daemon::audit_on_shard(|state| {
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
