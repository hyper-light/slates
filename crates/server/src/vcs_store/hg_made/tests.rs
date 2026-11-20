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
  let forked = take_whole(state, repository, part, &mut Admission::new(u64::MAX))
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
    crate::vcs_store::rebuild(state, repository);
    assert_eq!(
      head_of(state, green),
      Some(second),
      "the made node from the record"
    );
    for node in [first, pushed, second] {
      assert!(
        parent_of(state, repository, &node).is_some(),
        "{node:?} not held after the rebuild"
      );
    }
  });
}
