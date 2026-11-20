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
      default_branch: "default".to_owned(),
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
