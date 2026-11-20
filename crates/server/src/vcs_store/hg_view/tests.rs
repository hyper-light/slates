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

/// Lands `which` of `made` on repository `id` as a push of its owner lands them.
fn pushed(
  state: &mut ShardState,
  id: RepositoryId,
  made: &[crate::repository::hg_push::tests::Made],
  which: &[usize],
) {
  use slates_hg::push::{Asks, Taken, Unbundle};
  let unbundle = Unbundle {
    parts: vec![Taken {
      id: 0,
      mandatory: true,
      asks: Asks::Changegroup(crate::repository::hg_push::tests::changegroup(made, which)),
    }],
  };
  let mut room = Admission::new(u64::MAX);
  let terms = (false, [7; 32], u64::MAX);
  let prepared = crate::repository::hg_push::prepare_served(state, id, &unbundle, terms, &mut room)
    .expect("the push prepared");
  let landed = crate::repository::hg_push::land(state, (&owner(), id), prepared);
  assert!(
    landed.answer.is_ok() && landed.released.is_ok(),
    "{landed:?}"
  );
}

/// A-745 (§4.17.1 "A fetch's answer", rule 4). Do: push three changesets to a Mercurial repository; answer a clone of it
/// whole; then begin the same clone streamed, holding the generation it began on, and take one turn; land a fourth
/// changeset and put a rebuild of the store in flight; take the clone's remaining turns over its hold. Expect the
/// repository's own view not lent while the rebuild is in flight (a turn over it would fail the clone), and the
/// streamed answer, every turn over the held generation, the whole answer byte for byte.
#[test]
fn a_clone_reads_the_generation_it_began_on_whatever_lands_or_is_rebuilt_meanwhile() {
  use slates_hg::serve::http::{self as hg_http, Begun, Compression, Compressor, Media};
  struct Plain;
  impl Compressor for Plain {
    fn choose(&mut self, _: Media, _: &[slates_hg::compression::Engine], _: u64) -> Compression {
      Compression {
        engine: slates_hg::compression::Engine::Uncompressed,
        level: 0,
      }
    }
  }
  crate::daemon::audit_on_shard(|state| {
    let hg = created(state, "hg", VcsTool::Mercurial);
    let made = crate::repository::hg_push::tests::history_of(&[
      ("base", "a", b"a\n", None, "default"),
      ("second", "b", b"b\n", Some(0), "default"),
      ("third", "c", b"c\n", Some(1), "default"),
      ("fourth", "d", b"d\n", Some(2), "default"),
    ]);
    pushed(state, hg, &made, &[0, 1, 2]);
    let heads = crate::verbs::hex_of(&made[2].node);
    let arguments = format!(
      "bundlecaps=HG20%2Cbundle2%3DHG20%250Achangegroup%253D02%252C03&cg=1&common={}&heads={heads}",
      "0".repeat(40)
    );
    let headers: [(&[u8], &[u8]); 2] = [
      (b"X-HgArg-1", arguments.as_bytes()),
      (b"X-HgProto-1", b"0.1 0.2 comp=none"),
    ];
    let request = slates_hg::http::Request {
      query: b"cmd=getbundle",
      headers: &headers,
      body: &[],
    };
    let room = || Admission::new(ROOM);
    let whole = lend_in(state, hg, &mut room(), |view, left| {
      hg_http::answer(view, &request, 0, &mut Plain, left)
    })
    .expect("the whole answer");
    assert_eq!(whole.refused, None);
    let begun = lend_in(state, hg, &mut room(), |view, left| {
      hg_http::begin(view, &request, 0, &mut Plain, left)
    })
    .expect("the answer begun");
    let Begun::Streamed(mut streamed) = begun else {
      panic!("a clone's answer is streamed");
    };
    let generation = super::hold_in(state, hg, &mut room()).expect("the generation held");
    let mut body = Vec::new();
    let mut turn = |state: &mut ShardState, body: &mut Vec<u8>| {
      super::lend_held_in(state, &generation, &mut room(), |view, left| {
        streamed.step(view, left, body)
      })
      .expect("a turn over the held generation")
      .expect("a slice")
    };
    assert!(turn(state, &mut body));
    pushed(state, hg, &made, &[3]);
    state.vcs_stores.rebuilding.insert(hg);
    let rebuilding = lend_in(state, hg, &mut room(), |_, _| ());
    while turn(state, &mut body) {}
    state.vcs_stores.rebuilding.remove(&hg);
    generation.give_back(state);
    assert_eq!(
      rebuilding, None,
      "the repository's view was lent mid-rebuild"
    );
    assert_eq!(body, whole.body);
  });
}

/// A-575 (main's ruling of 2026-11-20: `--from` follows Mercurial's own name lookup, a bookmark, then a tag, then a
/// branch). Do: push a changeset to a Mercurial repository and branch `mark` from `default` at it; push two more, the
/// second adding `.hgtags` with `v1` at the first changeset and `mid` at the second; then branch from `default`, from
/// `tip`, from `v1`, from `mid` and from `nope`. Expect `default` and `tip` the line's head (the third changeset), as
/// Mercurial resolves a branch's name to its tip-most head; `v1` the first changeset, made from `mark`, the bookmark that
/// heads it; `mid` refused `Unsupported`, since no line or bookmark heads it; and `nope` `NotFound`.
#[test]
fn a_branch_from_a_name_follows_mercurials_lookup_bookmark_tag_then_branch() {
  use slates_ipc::protocol::Refusal;
  crate::daemon::audit_on_shard(|state| {
    let hg = created(state, "hg", VcsTool::Mercurial);
    let first = crate::repository::hg_push::tests::history_of(&[
      ("base", "a", b"a\n", None, "default"),
      ("second", "b", b"b\n", Some(0), "default"),
    ]);
    let tags = format!(
      "{} v1\n{} mid\n",
      crate::verbs::hex_of(&first[0].node),
      crate::verbs::hex_of(&first[1].node)
    );
    let tags: &'static [u8] = Box::leak(tags.into_bytes().into_boxed_slice());
    let made = crate::repository::hg_push::tests::history_of(&[
      ("base", "a", b"a\n", None, "default"),
      ("second", "b", b"b\n", Some(0), "default"),
      ("Added tags", ".hgtags", tags, Some(1), "default"),
    ]);
    let branch = |state: &mut ShardState, name: &str, from: &str| {
      dispatch(
        state,
        1,
        &owner(),
        RequestBody::CreateBranch {
          repository: "hg".to_owned(),
          name: name.to_owned(),
          from: from.to_owned(),
          version: None,
        },
      )
    };
    pushed(state, hg, &made, &[0]);
    let mark = branch(state, "mark", "default");
    assert!(matches!(mark, ReplyBody::GreenCreated { .. }), "{mark:?}");
    pushed(state, hg, &made, &[1, 2]);
    let replies: Vec<ReplyBody> = [
      ("from-default", "default"),
      ("from-tip", "tip"),
      ("from-v1", "v1"),
    ]
    .iter()
    .map(|(name, from)| branch(state, name, from))
    .collect();
    assert!(
      replies
        .iter()
        .all(|reply| matches!(reply, ReplyBody::GreenCreated { .. })),
      "{replies:?}"
    );
    let marks = bookmarks_of(state, hg);
    let at = |name: &[u8]| {
      marks
        .iter()
        .find(|(held, _)| held.as_slice() == name)
        .map(|(_, node)| *node)
    };
    assert_eq!(
      (
        at(b"mark"),
        at(b"from-default"),
        at(b"from-tip"),
        at(b"from-v1")
      ),
      (
        Some(made[0].node),
        Some(made[2].node),
        Some(made[2].node),
        Some(made[0].node)
      ),
      "{marks:?}"
    );
    let mid = branch(state, "from-mid", "mid");
    assert!(
      matches!(
        &mid,
        ReplyBody::Refused {
          refusal: Refusal::Unsupported { .. }
        }
      ),
      "{mid:?}"
    );
    assert_eq!(
      branch(state, "from-nope", "nope"),
      ReplyBody::Refused {
        refusal: Refusal::NotFound
      }
    );
  });
}
