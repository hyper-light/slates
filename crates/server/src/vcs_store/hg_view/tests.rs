//! A-680 part 2's tests of the lender: a Mercurial repository's served view lent from its store on the shard and its
//! register, and not lent where the map holds no store, an import is filling it, or the view is past the room.

use slates_db::catalog::{Principal, RepositoryId};
use slates_hg::Admission;
use slates_hg::import::Clock;
use slates_hg::serve::Repository;
use slates_ipc::protocol::{ObjectFormat as WireFormat, ReplyBody, RequestBody, VcsTool};

use super::{UNMADE, lend_in};
use crate::hg_base::tests::{STATES, laid_out};
use crate::state::ShardState;
use crate::vcs_store::hg_import::{Turned, begin, land, turn};
use crate::verbs::dispatch;

/// Shape: room enough for the view of every fixture these tests import.
const ROOM: u64 = 1 << 24;
/// Shape: more turns than the fixture's import takes, so an import that never ends fails the test rather than hangs it.
const TURNS_AT_MOST: u64 = 1 << 16;
/// Shape: the changesets stock 7.2's `served` filter serves of the `states` repository (`fixtures/state/states-served.txt`,
/// three lines): what its view serves once the import has landed its phase roots and bookmarks in the register (A-687).
const STATES_SERVED: u32 = 3;

/// A clock one nanosecond on at each reading, so every turn of an import passes its quantum of one nanosecond.
struct Ticking(u64);

impl Clock for Ticking {
  fn now_ns(&mut self) -> u64 {
    self.0 = self.0.saturating_add(1);
    self.0
  }
}

/// The repositories' owner.
fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// Creates repository `name` of `tool` on the shard: its id.
fn created(state: &mut ShardState, name: &str, tool: VcsTool) -> RepositoryId {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool,
      format: WireFormat::Sha1,
      // The root's name as the command line spells it (`repository::root_binding`): the first line of `default`.
      default_branch: "branch:default:0".to_owned(),
      green: None,
    },
  );
  assert!(
    matches!(reply, ReplyBody::GreenCreated { .. }),
    "no repository: {reply:?}"
  );
  let repositories = state.db.partition().repositories();
  repositories.repository_named(name).expect("the record").id
}

/// Runs repository `id`'s import, begun, a unit a turn to its end.
fn imported(state: &mut ShardState, id: RepositoryId) {
  let mut clock = Ticking(0);
  for _ in 0..TURNS_AT_MOST {
    match turn(state, id, (&mut clock, 1)) {
      Some(Ok(Turned::More)) => {}
      Some(Ok(Turned::Done(_))) => {
        assert_eq!(land(state, id), Ok(()));
        return;
      }
      other => panic!("the import did not end: {other:?}"),
    }
  }
  panic!("no end after {TURNS_AT_MOST} turns");
}

/// The count of the view of repository `id` the lender lends within `room`; none where it lends none.
fn lent_count(state: &mut ShardState, id: RepositoryId, room: u64) -> Option<u32> {
  lend_in(state, id, &mut Admission::new(room), |view, _| view.count())
}

/// A-680 part 2. Do: on one shard create a Mercurial repository and a git one; begin importing `fixtures/state`'s
/// `states` repository into the Mercurial one and ask for its view, then run the import to its end and ask again, then
/// ask for the git repository's, for an id no repository has, and for the Mercurial one's within a room of one byte.
/// Expect: nothing lent while the import fills the store; then, the import landed, the view of the imported history,
/// the changesets stock 7.2 serves (the register holds the phase roots and bookmarks the import found, A-687); nothing for the git repository and the unknown id, which count nothing; and
/// nothing past the room, counted as a view not made.
#[test]
fn t_a680_a_view_is_lent_from_the_store_or_not_at_all() {
  crate::daemon::audit_on_shard(|state| {
    let hg = created(state, "hg", VcsTool::Mercurial);
    let git = created(state, "git", VcsTool::Git);
    let (host, directory) = laid_out(STATES.iter().copied());
    assert_eq!(begin(state, hg, (Box::new(host), directory)), Ok(()));
    let while_importing = lent_count(state, hg, ROOM);
    imported(state, hg);
    let unmade = |state: &ShardState| state.refusals.get(UNMADE).copied().unwrap_or(0);
    let before = unmade(state);
    let lent = lent_count(state, hg, ROOM);
    let others = (
      lent_count(state, git, ROOM),
      lent_count(
        state,
        RepositoryId {
          bytes: [u8::MAX; 16],
        },
        ROOM,
      ),
    );
    let past_room = lent_count(state, hg, 1);
    assert_eq!(
      (while_importing, lent, others, past_room),
      (None, Some(STATES_SERVED), (None, None), None)
    );
    assert_eq!(unmade(state), before.saturating_add(1));
  });
}

/// The bookmarks repository `id`'s served view lists, each name with its node.
fn bookmarks_of(state: &mut ShardState, id: RepositoryId) -> Vec<(Vec<u8>, slates_hg::Node)> {
  lend_in(state, id, &mut Admission::new(ROOM), |view, _| {
    view
      .bookmarks()
      .map(|(name, node)| (name.to_vec(), node))
      .collect()
  })
  .expect("a view")
}

/// A-575 with §4.17.1's table (a slates branch is a Mercurial bookmark, names one to one; found by use for git, checked
/// here for Mercurial). Do: import `states` (bookmarks `one` and `two`), then by the owner's verbs, as the command line
/// sends them, create branch `topic` from `one`, retire it, and bind it again to its green. Expect `topic` listed as a
/// bookmark at `one`'s node once created, gone once retired, and back at it once bound; `one` and `two` unmoved.
#[test]
fn a_branch_the_owner_creates_retires_and_binds_is_a_bookmark_by_the_same_name() {
  crate::daemon::audit_on_shard(|state| {
    let hg = created(state, "hg", VcsTool::Mercurial);
    let (host, directory) = laid_out(STATES.iter().copied());
    assert_eq!(begin(state, hg, (Box::new(host), directory)), Ok(()));
    imported(state, hg);
    let before = bookmarks_of(state, hg);
    let one = before
      .iter()
      .find(|(name, _)| name.as_slice() == b"one")
      .map(|(_, node)| *node)
      .expect("states' bookmark one");
    let ask = |state: &mut ShardState, body: RequestBody| dispatch(state, 1, &owner(), body);
    let made = ask(
      state,
      RequestBody::CreateBranch {
        repository: "hg".to_owned(),
        name: "topic".to_owned(),
        from: "one".to_owned(),
        version: None,
      },
    );
    let ReplyBody::GreenCreated { id: green } = made else {
      panic!("no branch: {made:?}");
    };
    let topic = (b"topic".to_vec(), one);
    let created = bookmarks_of(state, hg);
    let retired_reply = ask(
      state,
      RequestBody::RetireBranch {
        repository: "hg".to_owned(),
        name: "topic".to_owned(),
      },
    );
    let retired = bookmarks_of(state, hg);
    let bound_reply = ask(
      state,
      RequestBody::BindBranch {
        repository: "hg".to_owned(),
        name: "topic".to_owned(),
        green,
      },
    );
    let bound = bookmarks_of(state, hg);
    assert_eq!(
      (retired_reply, bound_reply),
      (ReplyBody::Acknowledged, ReplyBody::Acknowledged)
    );
    assert_eq!(
      (
        created.contains(&topic),
        retired.iter().any(|(name, _)| name.as_slice() == b"topic"),
        bound.contains(&topic),
      ),
      (true, false, true),
      "created {created:?}, retired {retired:?}, bound {bound:?}"
    );
    assert_eq!(retired, before);
  });
}
