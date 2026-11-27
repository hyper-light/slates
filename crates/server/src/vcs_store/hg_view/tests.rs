//! A-680 part 2's tests of the lender: a Mercurial repository's served view lent from its store on the shard and its
//! register, and not lent where the map holds no store, an import is filling it, or the view is past the room.

use slates_db::catalog::{Principal, RepositoryId};
use slates_hg::Admission;
use slates_hg::serve::Repository;
use slates_hg::serve::getbundle::Store as _;
use slates_ipc::protocol::{ObjectFormat as WireFormat, ReplyBody, RequestBody, VcsTool};

use super::{UNMADE, lend_in};
use crate::hg_base::tests::{FILE_INDEX, Files, STATES};
use crate::repository::svn_commit::tests::on;
use crate::state::ShardState;
use crate::verbs::dispatch;

/// Shape: room enough for the view of every fixture these tests import.
const ROOM: u64 = 1 << 24;
/// Shape: the changesets stock 7.2's `served` filter serves of the `states` repository (`fixtures/state/states-served.txt`,
/// three lines): what its view serves once the import has landed its phase roots and bookmarks in the register (A-687).
const STATES_SERVED: u32 = 3;

/// The repositories' owner.
fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// Creates repository `name` of `tool` as a client's request does, from a task on the shard (`test_store::created`): its
/// id.
async fn created(name: &str, tool: VcsTool) -> RepositoryId {
  // The root's name as the command line spells it (`repository::root_binding`): the first line of `default`.
  let asked = (name, tool, WireFormat::Sha1, "branch:default:0");
  crate::test_store::created(&owner(), asked).await.0
}

/// Repository `name` (`id`) imported from `files` through the import verb as request `request`, awaited to its end
/// (`test_store::imported_through_the_verb`).
async fn imported(request: u64, (name, id): (&str, RepositoryId), files: Files) {
  crate::test_store::imported_through_the_verb((&owner(), request), (name, id), files).await;
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
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = created("hg", VcsTool::Mercurial).await;
    let git = created("git", VcsTool::Git).await;
    let while_importing = on(|state| {
      let begun = crate::test_store::import_begun(state, (&owner(), 2), "hg", STATES);
      assert_eq!(begun, ReplyBody::Acknowledged, "the import taken");
      lent_count(state, hg, ROOM)
    });
    crate::test_store::import_ended("hg", hg).await;
    on(|state| {
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
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = created("hg", VcsTool::Mercurial).await;
    imported(2, ("hg", hg), STATES).await;
    on(|state| {
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
  });
}

/// Pushes `which` of `made` to repository `id` as its owner's push does, through the daemon's driver
/// (`hg_push::unbundle`), from a task on the shard.
async fn pushed(
  id: RepositoryId,
  made: &[crate::repository::hg_push::tests::Made],
  which: &[usize],
) {
  let part = crate::repository::hg_push::tests::changegroup(made, which);
  crate::repository::hg_push::tests::pushed_changegroup(id, part).await;
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
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = created("hg", VcsTool::Mercurial).await;
    let made = crate::repository::hg_push::tests::history_of(&[
      ("base", "a", b"a\n", None, "default"),
      ("second", "b", b"b\n", Some(0), "default"),
      ("third", "c", b"c\n", Some(1), "default"),
      ("fourth", "d", b"d\n", Some(2), "default"),
    ]);
    pushed(hg, &made, &[0, 1, 2]).await;
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
    let begin = |state: &mut ShardState| {
      let begun = lend_in(state, hg, &mut room(), |view, left| {
        hg_http::begin(view, &request, 0, &mut Plain, left)
      })
      .expect("the answer begun");
      let Begun::Streamed(streamed) = begun else {
        panic!("a clone's answer is streamed");
      };
      streamed
    };
    // The whole answer as the edges make it: a step at a time, each want of the changeset store answered between steps
    // (A-767 C1: a clone's file revisions as the store keeps them).
    let mut streamed = on(begin);
    let mut whole = Vec::new();
    loop {
      assert!(super::answer_store_wants(&mut *streamed, hg).await);
      let more = on(|state| {
        lend_in(state, hg, &mut room(), |view, left| {
          streamed.step(view, left, &mut whole)
        })
      })
      .expect("a step")
      .expect("a slice");
      if !more {
        break;
      }
    }
    let (mut streamed, generation) = on(|state| {
      let streamed = begin(state);
      let generation = super::hold_in(state, hg, &mut room()).expect("the generation held");
      (streamed, generation)
    });
    let mut body = Vec::new();
    let mut turn = async |body: &mut Vec<u8>| {
      assert!(super::answer_store_wants(&mut *streamed, hg).await);
      on(|state| {
        super::lend_held_in(state, &generation, &mut room(), |view, left| {
          streamed.step(view, left, body)
        })
      })
      .expect("a turn over the held generation")
      .expect("a slice")
    };
    assert!(turn(&mut body).await);
    pushed(hg, &made, &[3]).await;
    let rebuilding = on(|state| {
      state.vcs_stores.rebuilding.insert(hg);
      lend_in(state, hg, &mut room(), |_, _| ())
    });
    while turn(&mut body).await {}
    on(|state| {
      state.vcs_stores.rebuilding.remove(&hg);
      generation.give_back(state);
    });
    assert_eq!(
      rebuilding, None,
      "the repository's view was lent mid-rebuild"
    );
    assert_eq!(body, whole);
  });
}

/// A-575 (main's ruling of 2026-11-20: `--from` follows Mercurial's own name lookup, a bookmark, then a tag, then a
/// branch). Do: push a changeset to a Mercurial repository and branch `mark` from `default` at it; push two more, the
/// second adding `.hgtags` with `v1` at the first changeset and `mid` at the second; then branch from `default`, from
/// `tip`, from `v1`, from `mid` and from `nope`. Expect `default` and `tip` the line's head (the third changeset), as
/// Mercurial resolves a branch's name to its tip-most head; `v1` the first changeset, made from `mark`, the bookmark that
/// heads it; `mid` refused `Unsupported`, since no line or bookmark heads it; and `nope` `NotFound`. Since A-767 C1
/// (main's ruling (A)) a name that reaches the tags has the heads' `.hgtags` read from the changeset store at the verb's
/// door first: a read for `default` and for `v1`, none for `tip`. And `mark`'s from `default` reads its head's manifest,
/// let go once its push landed as production lets one go, to learn that it names no `.hgtags` yet.
#[test]
fn a_branch_from_a_name_follows_mercurials_lookup_bookmark_tag_then_branch() {
  use slates_ipc::protocol::Refusal;
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = created("hg", VcsTool::Mercurial).await;
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
    let reads = core::cell::RefCell::new(Vec::new());
    let branch = async |name: &str, from: &str| {
      let (reply, read) = crate::vcs_store::hg_tags::dispatch_awaited(
        &owner(),
        RequestBody::CreateBranch {
          repository: "hg".to_owned(),
          name: name.to_owned(),
          from: from.to_owned(),
          version: None,
        },
      )
      .await;
      reads.borrow_mut().push((name.to_owned(), read));
      reply
    };
    pushed(hg, &made, &[0]).await;
    let mark = branch("mark", "default").await;
    assert!(matches!(mark, ReplyBody::GreenCreated { .. }), "{mark:?}");
    pushed(hg, &made, &[1, 2]).await;
    let mut replies: Vec<ReplyBody> = Vec::new();
    for (name, from) in [
      ("from-default", "default"),
      ("from-tip", "tip"),
      ("from-v1", "v1"),
    ] {
      replies.push(branch(name, from).await);
    }
    assert!(
      replies
        .iter()
        .all(|reply| matches!(reply, ReplyBody::GreenCreated { .. })),
      "{replies:?}"
    );
    let marks = on(|state| bookmarks_of(state, hg));
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
    // `default` and `v1` reach the tags, read from the changeset store before each runs (A-767 C1); `tip` never. `mark`'s
    // read is its head's manifest, let go once its push landed, read back to learn it names no `.hgtags` yet.
    let read = |name: &str| {
      reads
        .borrow()
        .iter()
        .find(|(made, _)| made == name)
        .map(|(_, read)| *read)
    };
    assert_eq!(
      [
        read("mark"),
        read("from-default"),
        read("from-tip"),
        read("from-v1")
      ],
      [Some(true), Some(true), Some(false), Some(true)],
      "the tags read where a name reaches them, never for `tip`"
    );
    let mid = branch("from-mid", "mid").await;
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
      branch("from-nope", "nope").await,
      ReplyBody::Refused {
        refusal: Refusal::NotFound
      }
    );
  });
}

/// Goal 93, C1 (1) P5 (main's hg_interop round at a30ffbdb8: `stock_hg_pulls_by_a_tag_over_http_and_a_branch_is_made_from_
/// the_tag` hung, its `branch create --from v1` parked again and again). Do: push a changeset to a Mercurial repository and
/// branch `mark` from `default` at it; push two more, the second adding `.hgtags` with `v1` at the first; let go every
/// object the repository's changelog and manifest rows name, as a landing does since P5; branch `from-v1` from `v1`
/// through the verb's door. Expect the branch made at the first changeset, its tags read from the changeset store once.
/// Red before: the door's read, answered, did not answer the verb that ran again over it: a read begun over heads whose
/// manifests are not at hand knows no `.hgtags` revision yet, so it never matched the read held (`TagsRead::same_files`),
/// and the verb was refused (in the daemon, parked again, for ever).
#[test]
fn a_branch_from_a_tag_whose_heads_manifests_were_let_go_reads_them_once_and_is_made() {
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = created("hg", VcsTool::Mercurial).await;
    let first = crate::repository::hg_push::tests::history_of(&[
      ("base", "a", b"a\n", None, "default"),
      ("second", "b", b"b\n", Some(0), "default"),
    ]);
    let tags = format!("{} v1\n", crate::verbs::hex_of(&first[0].node));
    let tags: &'static [u8] = Box::leak(tags.into_bytes().into_boxed_slice());
    let made = crate::repository::hg_push::tests::history_of(&[
      ("base", "a", b"a\n", None, "default"),
      ("second", "b", b"b\n", Some(0), "default"),
      ("Added tags", ".hgtags", tags, Some(1), "default"),
    ]);
    let branch = async |name: &str, from: &str| {
      crate::vcs_store::hg_tags::dispatch_awaited(
        &owner(),
        RequestBody::CreateBranch {
          repository: "hg".to_owned(),
          name: name.to_owned(),
          from: from.to_owned(),
          version: None,
        },
      )
      .await
    };
    pushed(hg, &made, &[0]).await;
    let (mark, _) = branch("mark", "default").await;
    assert!(matches!(mark, ReplyBody::GreenCreated { .. }), "{mark:?}");
    pushed(hg, &made, &[1, 2]).await;
    let let_go = on(|state| {
      crate::vcs_store::lent_state(state, |state| {
        crate::vcs_store::within_budget(state, 0, |stores| {
          let (content, store) = stores.mercurial(hg)?;
          let index = &store.index;
          let changesets = (0..index.len(content, slates_hg::index::Log::Changelog))
            .map(|rev| index.changeset(content, rev).map(|row| row.text));
          let manifests = (0..index.len(content, slates_hg::index::Log::Manifest)).map(|rev| {
            index
              .revision(content, slates_hg::index::Log::Manifest, rev)
              .map(|row| row.bytes)
          });
          let objects = changesets.chain(manifests).collect::<Option<Vec<_>>>()?;
          for object in &objects {
            content.let_go_object(object);
          }
          Some(objects.iter().all(|object| !content.holds_whole(object)))
        })
      })
    });
    // Let go by its landing as production lets a landed text go, or by this let-go where the landing kept it.
    assert_eq!(
      let_go,
      Some(true),
      "every changelog and manifest object let go"
    );
    let (from_v1, read) = branch("from-v1", "v1").await;
    assert!(
      matches!(from_v1, ReplyBody::GreenCreated { .. }),
      "{from_v1:?}"
    );
    assert!(read, "the tags read from the changeset store");
    let marks = on(|state| bookmarks_of(state, hg));
    let at = marks
      .iter()
      .find(|(held, _)| held.as_slice() == b"from-v1")
      .map(|(_, node)| *node);
    assert_eq!(at, Some(made[0].node), "{marks:?}");
  });
}

/// A-575 with A-767 C1, by use (main's hg_interop round at b8ba82ad3: `stock_hg_pulls_by_a_tag_over_http_and_a_branch_is_
/// made_from_the_tag` refused "a branch made at `v1` ... which no line or bookmark of the repository heads"). Do: push
/// `first`, then `.hgtags` naming `v1` at `first`, then `third`, as the interop test's `hg tag --rev 0 v1` and commits
/// make them; push bookmark `mark` at `first`, behind `default`'s head, as `hg push -B mark` sends it; then branch
/// `from-tag` from `v1` through the verb's door. Expect the bookmark's push accepted and `mark` heading `first`, then the
/// branch made at `first`.
#[test]
fn a_bookmark_pushed_at_a_tagged_changeset_behind_its_lines_head_heads_it_and_a_branch_is_made_from_the_tag()
 {
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = created("hg", VcsTool::Mercurial).await;
    let first = crate::repository::hg_push::tests::history_of(&[(
      "first", "file", b"first\n", None, "default",
    )]);
    let tags = format!("{} v1\n", crate::verbs::hex_of(&first[0].node));
    let tags: &'static [u8] = Box::leak(tags.into_bytes().into_boxed_slice());
    let made = crate::repository::hg_push::tests::history_of(&[
      ("first", "file", b"first\n", None, "default"),
      ("Added tag v1", ".hgtags", tags, Some(0), "default"),
      ("third", "third", b"third\n", Some(1), "default"),
    ]);
    pushed(hg, &made, &[0, 1, 2]).await;
    let marked =
      crate::repository::hg_push::tests::pushed_bookmark(hg, b"mark", made[0].node).await;
    assert!(
      matches!(marked, slates_hg::push::Outcome::Accepted(_)),
      "push -B mark: {marked:?}"
    );
    let mark = on(|state| {
      let green = state
        .db
        .partition()
        .repositories()
        .branch(hg, &crate::repository::hg_push::bookmark_name(b"mark"))?;
      Some(crate::vcs_store::hg_made::head_of(state, green))
    });
    assert_eq!(mark, Some(Some(made[0].node)), "`mark` heads `first`");
    let (from_tag, _) = crate::vcs_store::hg_tags::dispatch_awaited(
      &owner(),
      RequestBody::CreateBranch {
        repository: "hg".to_owned(),
        name: "from-tag".to_owned(),
        from: "v1".to_owned(),
        version: None,
      },
    )
    .await;
    assert!(
      matches!(from_tag, ReplyBody::GreenCreated { .. }),
      "{from_tag:?}"
    );
    let marks = on(|state| bookmarks_of(state, hg));
    let at = marks
      .iter()
      .find(|(held, _)| held.as_slice() == b"from-tag")
      .map(|(_, node)| *node);
    assert_eq!(at, Some(made[0].node), "{marks:?}");
  });
}

/// Whether repository `id`'s view advertises a stream clone (`stream`/`streamreqs` in its capabilities), lent now and
/// lent over a held generation: both, which must agree.
fn streams_of(state: &mut ShardState, id: RepositoryId) -> (bool, bool) {
  let room = || Admission::new(ROOM);
  let lent = lend_in(state, id, &mut room(), |view, _| view.streams()).expect("the view lent");
  let generation = super::hold_in(state, id, &mut room()).expect("the generation held");
  let held = super::lend_held_in(state, &generation, &mut room(), |view, _| view.streams())
    .expect("the held view lent");
  generation.give_back(state);
  (lent, held)
}

/// Goal 93 perf (main's ruling of 2026-11-21 02:31). A stream clone sends each revision as the changeset store keeps it,
/// so a repository whose history the store does not keep must advertise no stream, and stock `hg clone --stream` then
/// clones plainly with its own warning rather than abort ("operation forbidden by server"). Since w-hg's C1 step 3 an
/// import lands on the store as segment changesets, so an imported repository's history is the store's as a pushed
/// one's is. Do: import `fileindex` (public, no secret changeset, so nothing else withholds its stream) into one
/// Mercurial repository and push three changesets to another; ask each view, lent now and over a held generation,
/// whether it streams. Expect both to, either way.
#[test]
fn a_stream_clone_is_advertised_only_where_the_store_keeps_the_repositorys_logs() {
  crate::test_store::run_on_stored_shard(|| async move {
    let imported_id = created("imported", VcsTool::Mercurial).await;
    imported(2, ("imported", imported_id), FILE_INDEX).await;
    let pushed_id = created("pushed", VcsTool::Mercurial).await;
    let made = crate::repository::hg_push::tests::history_of(&[
      ("base", "a", b"a\n", None, "default"),
      ("second", "b", b"b\n", Some(0), "default"),
      ("third", "c", b"c\n", Some(1), "default"),
    ]);
    pushed(pushed_id, &made, &[0, 1, 2]).await;
    assert_eq!(
      on(|state| (streams_of(state, imported_id), streams_of(state, pushed_id))),
      ((true, true), (true, true))
    );
  });
}

/// A want of whole texts made by the store ([`slates_hg::render::Want::Texts`]) and what it was given.
struct AskedTexts {
  /// The want, until answered.
  want: Option<slates_hg::render::Want>,
  /// The texts given, or `None` where the store found them past the want's room.
  given: Option<Option<Vec<Vec<u8>>>>,
}

impl slates_hg::render::StoreWants for AskedTexts {
  fn wants(&self) -> Option<&slates_hg::render::Want> {
    self.want.as_ref()
  }

  fn give_rows(&mut self, _rows: Vec<Vec<Option<slates_hg::render::StoredRevision>>>) {
    self.want = None;
  }

  fn give_chunks(&mut self, _chunks: Vec<Vec<u8>>) {
    self.want = None;
  }

  fn give_texts(&mut self, texts: Vec<Vec<u8>>) {
    self.want = None;
    self.given = Some(Some(texts));
  }

  fn texts_past(&mut self) {
    self.want = None;
    self.given = Some(None);
  }
}

/// A-767 C1 (the store makes a Mercurial text whole: a pull's revision sent whole, a lookup's `.hgtags`). Do: push two
/// changesets to a Mercurial repository, each adding a file, and ask the node's store for both files' texts whole
/// (`hg_view::texts_made`, `StoreHandle::text`), then again within one byte less than they hold. Expect each text as it
/// was pushed, then the read refused past its room: a whole a push keeps is its raw text, as the store makes it.
#[test]
fn the_store_makes_a_pushed_mercurial_text_whole() {
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = created("hg", VcsTool::Mercurial).await;
    let made = crate::repository::hg_push::tests::history_of(&[
      ("base", "a", b"a\n", None, "default"),
      ("second", "b", b"b, a longer text\n", Some(0), "default"),
    ]);
    pushed(hg, &made, &[0, 1]).await;
    let texts: Vec<[u8; 32]> = [&b"a\n"[..], b"b, a longer text\n"]
      .iter()
      .map(|text| *blake3::hash(text).as_bytes())
      .collect();
    let ask = async |most: u64| {
      let mut asked = AskedTexts {
        want: Some(slates_hg::render::Want::Texts {
          texts: texts.clone(),
          most,
        }),
        given: None,
      };
      assert!(super::answer_store_wants(&mut asked, hg).await);
      asked.given.expect("an answer")
    };
    assert_eq!(
      ask(u64::MAX).await,
      Some(vec![b"a\n".to_vec(), b"b, a longer text\n".to_vec()])
    );
    let held = u64::try_from(b"a\n".len() + b"b, a longer text\n".len()).unwrap();
    assert_eq!(ask(held - 1).await, None, "refused past its room");
  });
}

/// Shape: the step's copy budget the sliced read is held to: far under its text's bytes.
const SMALL_STEP: u64 = 1 << 10;
/// Shape: the lines of the text read in slices, about 40 bytes each: a dozen steps of [`SMALL_STEP`].
const LONG_TEXT_LINES: usize = 300;

/// Goal 93 (main's ruling of 2026-11-25: a fetch's texts copied by the store's owner at most a step's copy budget a
/// request, a text past it in slices across requests; at hg's 57k clone one request copied a whole batch of the
/// fetch's room). Do: push a changeset adding a file of [`LONG_TEXT_LINES`] lines, and ask the node's store for its text
/// whole within a step of [`SMALL_STEP`] bytes (`hg_view::texts_made`). Expect the text as pushed; no request copying
/// more than the step; as many requests as its slices. Red before: one request copying the whole text.
#[test]
fn a_text_past_a_step_is_copied_a_step_a_request() {
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = created("hg", VcsTool::Mercurial).await;
    let long: &'static [u8] = Box::leak(
      (0..LONG_TEXT_LINES)
        .flat_map(|line| format!("line {line} of a text several steps long\n").into_bytes())
        .collect::<Vec<u8>>()
        .into_boxed_slice(),
    );
    let made =
      crate::repository::hg_push::tests::history_of(&[("base", "long", long, None, "default")]);
    pushed(hg, &made, &[0]).await;
    let identity = *blake3::hash(long).as_bytes();
    let disk = on(|state| state.vcs_disk.clone()).expect("a store");
    let repository = u128::from_be_bytes(hg.bytes);
    let (made, asks) = super::texts_made(&disk, repository, &[identity], (u64::MAX, SMALL_STEP))
      .await
      .expect("the text made");
    let slices = (long.len() as u64).div_ceil(SMALL_STEP);
    println!(
      "hg_view: a text of {} B within a step of {SMALL_STEP} B: {asks:?} ({slices} slices)",
      long.len()
    );
    match made {
      super::MadeTexts::Made(made) => assert_eq!(made, vec![long.to_vec()], "the text as pushed"),
      super::MadeTexts::Past => panic!("refused past its room"),
    }
    assert!(
      asks.most_copied <= SMALL_STEP,
      "a request copied {} B, past the step's {SMALL_STEP}",
      asks.most_copied
    );
    assert!(asks.requests >= slices, "a request a slice: {asks:?}");
  });
}

/// Goal 93 (main's ruling of 2026-11-25, the fetch's step rule for a replay's base asks: a recovery asks the node's
/// changeset store for the texts its predecessor let go, through `hg_view::answer_wants_over`, which made a want of them
/// whole in one request of the owner whatever its bytes). Do: push a changeset adding a file of [`LONG_TEXT_LINES`]
/// lines; answer a want of its text whole, as a replay's base ask is answered, within a step of [`SMALL_STEP`] bytes.
/// Expect the text as pushed; no request copying more than the step; as many requests as its slices. Red before: one
/// request copying the whole text.
#[test]
fn a_replays_ask_past_a_step_is_copied_a_step_a_request() {
  crate::test_store::run_on_stored_shard(|| async move {
    let hg = created("hg", VcsTool::Mercurial).await;
    let long: &'static [u8] = Box::leak(
      (0..LONG_TEXT_LINES)
        .flat_map(|line| format!("line {line} of a base a replay asks for\n").into_bytes())
        .collect::<Vec<u8>>()
        .into_boxed_slice(),
    );
    let made =
      crate::repository::hg_push::tests::history_of(&[("base", "long", long, None, "default")]);
    pushed(hg, &made, &[0]).await;
    let identity = *blake3::hash(long).as_bytes();
    let disk = on(|state| state.vcs_disk.clone()).expect("a store");
    let repository = u128::from_be_bytes(hg.bytes);
    let mut wanted = AskedTexts {
      want: Some(slates_hg::render::Want::Texts {
        texts: vec![identity],
        most: u64::MAX,
      }),
      given: None,
    };
    let asks = super::answer_wants_over(&disk, repository, &mut wanted, SMALL_STEP)
      .await
      .expect("every want answered");
    let asked = wanted.given;
    let slices = (long.len() as u64).div_ceil(SMALL_STEP);
    println!(
      "hg_view: a replay's ask of {} B within a step of {SMALL_STEP} B: {asks:?} ({slices} slices)",
      long.len()
    );
    assert_eq!(asked, Some(Some(vec![long.to_vec()])), "the text as pushed");
    assert!(
      asks.most_copied <= SMALL_STEP,
      "a request copied {} B, past the step's {SMALL_STEP}",
      asks.most_copied
    );
    assert!(asks.requests >= slices, "a request a slice: {asks:?}");
  });
}

/// What a revision's text must be: a changeset's or a manifest's text whole, as the same history imported in one call
/// into a content store of its own holds it ([`one_call_texts`]); a file revision's by the identities its row names
/// (its bytes' BLAKE3 and its metadata block's). A landed text leaves the node's content store once its writer lands it
/// (goal 93 item 2b(b), and every stored test's landing since main's ruling A3 of 2026-11-26), so no text the store
/// makes is checked against the store itself.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kept {
  /// A changeset's or a manifest's text.
  Text(Vec<u8>),
  /// A file revision's names.
  File(slates_hg::push::verify::FileNames),
}

impl Kept {
  /// Whether `text`, as the store made it, is the text kept.
  fn is(&self, text: &[u8]) -> bool {
    match self {
      Kept::Text(kept) => kept.as_slice() == text,
      Kept::File(names) => slates_hg::filerev::split(text).is_ok_and(|split| {
        slates_vcs::ContentId::of(split.bytes) == names.bytes
          && split.metadata.map(slates_vcs::ContentId::of) == names.block
      }),
    }
  }
}

/// Every revision of repository `id`'s served view: its log's path as the store's rows name it, its node and what its
/// text must be ([`Kept`]), changesets first, then the manifest log's, then each file log's, in order.
fn every_revision(
  state: &mut ShardState,
  id: RepositoryId,
  texts: &std::collections::BTreeMap<slates_hg::Node, Vec<u8>>,
) -> Vec<(Vec<u8>, slates_hg::Node, Kept)> {
  use slates_hg::push::intake::{CHANGELOG_PATH, MANIFEST_PATH};
  use slates_hg::push::verify::Held;
  let mut room = Admission::new(ROOM);
  let text_of =
    |node: &slates_hg::Node| Kept::Text(texts.get(node).expect("a text of the node").clone());
  crate::vcs_store::lent_state(state, |state| {
    super::with_view(state, id, &mut room, |view, held, _| {
      let mut revisions = Vec::new();
      for rev in 0..view.count() {
        let node = view.node(rev).unwrap();
        revisions.push((CHANGELOG_PATH.to_vec(), node, text_of(&node)));
      }
      let mut position = 0;
      while let Some(linked) = view.manifest_linked(position) {
        revisions.push((MANIFEST_PATH.to_vec(), linked.node, text_of(&linked.node)));
        position += 1;
      }
      let mut at = (Vec::new(), 0);
      while let Some((path, position, linked)) = view.file_linked_from((&at.0, at.1)) {
        // The served view reads no file's text (A-767 C1); the index's own names, as its writer kept them, are the oracle.
        let names = held.file_names(&path, &linked.node).unwrap();
        revisions.push((path.clone(), linked.node, Kept::File(names)));
        at = (path, position + 1);
      }
      revisions
    })
  })
  .expect("the view lent")
}

/// Every changeset's and manifest's text of `files`, imported in one call into a content store of its own that lets
/// nothing go (`slates_hg::import::import`), by node: the oracle [`every_revision`] holds the store's texts to.
fn one_call_texts(files: Files) -> std::collections::BTreeMap<slates_hg::Node, Vec<u8>> {
  use slates_hg::index::Log;
  let (mut host, repository) = crate::hg_base::tests::laid_out(files.iter().copied());
  let mut store = slates_vcs::ContentStore::new(ROOM, ONE_CALL_WINDOW).expect("a content store");
  let mut base = crate::hg_base::HgBase::new(&mut host, repository);
  let imported =
    slates_hg::import::import(&mut base, &mut store, Admission::new(ROOM)).expect("imported");
  let index = &imported.index;
  let mut texts = std::collections::BTreeMap::new();
  for rev in 0..index.len(&store, Log::Changelog) {
    let row = index.changeset(&store, rev).expect("a changeset");
    let text = store.object_bytes(&row.text, row.len).expect("its text");
    texts.insert(row.node, text);
  }
  for rev in 0..index.len(&store, Log::Manifest) {
    let node = index
      .revision(&store, Log::Manifest, rev)
      .expect("a manifest")
      .node;
    let text = index
      .revision_text(&store, Log::Manifest, rev)
      .expect("its text");
    texts.insert(node, text);
  }
  texts
}

/// Shape: the content store's window [`one_call_texts`] imports into: any, since it lets nothing go.
const ONE_CALL_WINDOW: usize = 1 << 16;

/// What the node's store keeps of `revisions` in repository `id`: how many it keeps as an edit against a base, and each
/// text made whole from its chain (`StoreHandle::texts_by_tool`, then `hg_view::texts_made`).
async fn kept_by_the_store(
  id: RepositoryId,
  revisions: &[(Vec<u8>, slates_hg::Node, Kept)],
) -> (usize, Vec<Vec<u8>>) {
  struct Rows {
    want: Option<slates_hg::render::Want>,
    rows: Vec<Vec<Option<slates_hg::render::StoredRevision>>>,
  }
  impl slates_hg::render::StoreWants for Rows {
    fn wants(&self) -> Option<&slates_hg::render::Want> {
      self.want.as_ref()
    }
    fn give_rows(&mut self, rows: Vec<Vec<Option<slates_hg::render::StoredRevision>>>) {
      self.want = None;
      self.rows = rows;
    }
    fn give_chunks(&mut self, _chunks: Vec<Vec<u8>>) {
      self.want = None;
    }
  }
  let logs = revisions
    .iter()
    .map(|(path, node, _)| (path.clone(), vec![*node]))
    .collect();
  let mut rows = Rows {
    want: Some(slates_hg::render::Want::Rows { logs }),
    rows: Vec::new(),
  };
  assert!(super::answer_store_wants(&mut rows, id).await);
  let rows: Vec<slates_hg::render::StoredRevision> = rows
    .rows
    .into_iter()
    .map(|log| {
      log
        .into_iter()
        .next()
        .flatten()
        .expect("a row for every revision")
    })
    .collect();
  let edits = rows.iter().filter(|row| row.base.is_some()).count();
  let mut asked = AskedTexts {
    want: Some(slates_hg::render::Want::Texts {
      texts: rows.iter().map(|row| row.text).collect(),
      most: ROOM,
    }),
    given: None,
  };
  assert!(super::answer_store_wants(&mut asked, id).await);
  (
    edits,
    asked.given.expect("an answer").expect("within the room"),
  )
}

/// The changegroup a clone of repository `id` sends over the HTTP edge, its file revisions as the store keeps them
/// (A-767 C1 step 1a), as a push's part.
async fn cloned_changegroup(id: RepositoryId) -> slates_hg::push::ChangegroupPart {
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
  let heads: Vec<String> = on(|state| {
    lend_in(state, id, &mut Admission::new(ROOM), |view, room| {
      slates_hg::serve::discovery::head_revs(view, room)
        .unwrap()
        .into_iter()
        .map(|rev| crate::verbs::hex_of(&view.node(rev).unwrap()))
        .collect()
    })
  })
  .expect("the view lent");
  let arguments = format!(
    "bundlecaps=HG20%2Cbundle2%3DHG20%250Achangegroup%253D02%252C03&cg=1&common={}&heads={}",
    "0".repeat(40),
    heads.join("+")
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
  let begun = on(|state| {
    lend_in(state, id, &mut Admission::new(ROOM), |view, left| {
      hg_http::begin(view, &request, 0, &mut Plain, left)
    })
  })
  .expect("the answer begun");
  let Begun::Streamed(mut streamed) = begun else {
    panic!("a clone's answer is streamed");
  };
  let mut body = Vec::new();
  loop {
    assert!(super::answer_store_wants(&mut *streamed, id).await);
    let more = on(|state| {
      lend_in(state, id, &mut Admission::new(ROOM), |view, left| {
        streamed.step(view, left, &mut body)
      })
    })
    .expect("a step")
    .expect("a slice");
    if !more {
      break;
    }
  }
  // The `0.2` media's header first: the compression engine's name, its length a byte before it.
  let engine = usize::from(body[0]);
  let stream = &body[1 + engine..];
  let mut room = Admission::new(ROOM);
  let bundle = slates_hg::bundle2::Bundle2::decode(stream, &mut room).expect("a bundle2 stream");
  let (index, part) = bundle
    .top_level()
    .find(|(_, part)| part.is(b"changegroup"))
    .expect("a changegroup part");
  let version =
    slates_hg::changegroup::Version::from_part_param(part.param(b"version")).expect("a version");
  slates_hg::push::ChangegroupPart {
    version,
    changesets: None,
    target_phase: None,
    tree_manifests: false,
    payload: bundle.payload_bytes(index, &mut room).expect("its payload"),
  }
}

/// A-767 C1 (main's ruling of 2026-11-21: a Mercurial whole's place is its raw text, the store's contract; the
/// two-encodings oracle). Do: for each of 7.2's five store formats (dotencode, hybrid, pre-share-safe, revlog v2,
/// file index), import the history into one repository (the import's writer); clone it through the store and push the
/// clone's changegroup into another (the push intake's writer); ask the node's store for every revision of each
/// whole (`StoreHandle::text`). Expect every text the store makes equal the text the view derives, revision for
/// revision (a file's, the bytes and block its row names, its text let go once landed), under both writers, and edits
/// among what each writer kept, so both a whole and a delta are made.
#[test]
fn every_text_both_writers_keep_is_made_whole_by_the_store_in_every_format() {
  use crate::hg_base::tests::{DOT, FILE_INDEX, HYBRID, LEGACY, V2};
  crate::test_store::run_on_stored_shard(|| async move {
    let formats = [
      ("dot", DOT),
      ("hybrid", HYBRID),
      ("legacy", LEGACY),
      ("v2", V2),
      ("fileindex", FILE_INDEX),
    ];
    for ((name, files), request) in formats.into_iter().zip(2..) {
      let imported_name = format!("{name}-imported");
      let imported_id = created(&imported_name, VcsTool::Mercurial).await;
      imported(request, (&imported_name, imported_id), files).await;
      let texts = one_call_texts(files);
      let revisions = on(|state| every_revision(state, imported_id, &texts));
      let made_as_kept = |made: &[Vec<u8>]| {
        made.len() == revisions.len()
          && made
            .iter()
            .zip(&revisions)
            .all(|(made, (_, _, kept))| kept.is(made))
      };
      let (imported_edits, made) = kept_by_the_store(imported_id, &revisions).await;
      assert!(made_as_kept(&made), "{name}: the import's texts made whole");
      let changegroup = cloned_changegroup(imported_id).await;
      let pushed_id = created(&format!("{name}-pushed"), VcsTool::Mercurial).await;
      push_part(pushed_id, changegroup).await;
      assert_eq!(
        on(|state| every_revision(state, pushed_id, &texts)),
        revisions,
        "{name}: the push holds the import's history"
      );
      let (pushed_edits, made) = kept_by_the_store(pushed_id, &revisions).await;
      assert!(made_as_kept(&made), "{name}: the push's texts made whole");
      assert!(
        imported_edits > 0 && pushed_edits > 0,
        "{name}: edits kept, {imported_edits} imported and {pushed_edits} pushed"
      );
    }
  });
}

/// Pushes `changegroup` to repository `id` as its owner's push does ([`pushed`]).
async fn push_part(id: RepositoryId, changegroup: slates_hg::push::ChangegroupPart) {
  crate::repository::hg_push::tests::pushed_changegroup(id, changegroup).await;
}
