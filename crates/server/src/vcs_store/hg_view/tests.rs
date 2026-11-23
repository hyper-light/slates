//! A-680 part 2's tests of the lender: a Mercurial repository's served view lent from its store on the shard and its
//! register, and not lent where the map holds no store, an import is filling it, or the view is past the room.

use slates_db::catalog::{Principal, RepositoryId};
use slates_hg::Admission;
use slates_hg::import::Clock;
use slates_hg::serve::Repository;
use slates_hg::serve::getbundle::Store as _;
use slates_ipc::protocol::{ObjectFormat as WireFormat, ReplyBody, RequestBody, VcsTool};

use super::{UNMADE, lend_in};
use crate::hg_base::tests::{FILE_INDEX, STATES, laid_out};
use crate::state::ShardState;
use crate::vcs_store::hg_import::{Turned, begin, land_imported, turn};
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
  let reply = crate::test_store::create(
    state,
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
        assert_eq!(land_imported(state, id), Ok(()));
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
  crate::test_store::audit_on_stored_shard("hg", |state| {
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
  crate::test_store::audit_on_stored_shard("hg", |state| {
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
  let landed = crate::repository::hg_push::land_stored(state, (&owner(), id), prepared);
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
  crate::test_store::audit_on_stored_shard("hg", |state| {
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
    let disk = state.vcs_disk.clone().expect("the store");
    let repository = u128::from_be_bytes(hg.bytes);
    // The whole answer as the edges make it: a step at a time, each want of the changeset store answered between steps
    // (A-767 C1: a clone's file revisions as the store keeps them).
    let whole = {
      let begun = lend_in(state, hg, &mut room(), |view, left| {
        hg_http::begin(view, &request, 0, &mut Plain, left)
      })
      .expect("the answer begun");
      let Begun::Streamed(mut streamed) = begun else {
        panic!("a clone's answer is streamed");
      };
      let mut body = Vec::new();
      loop {
        assert!(crate::test_store::answer_wants_blocking(
          &disk,
          repository,
          &mut *streamed
        ));
        let more = lend_in(state, hg, &mut room(), |view, left| {
          streamed.step(view, left, &mut body)
        })
        .expect("a step")
        .expect("a slice");
        if !more {
          break body;
        }
      }
    };
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
      assert!(crate::test_store::answer_wants_blocking(
        &disk,
        repository,
        &mut *streamed
      ));
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
/// door first: none read before the head names a `.hgtags`, then one read each for `default` and `v1`.
#[test]
fn a_branch_from_a_name_follows_mercurials_lookup_bookmark_tag_then_branch() {
  use slates_ipc::protocol::Refusal;
  crate::test_store::audit_on_stored_shard("hg", |state| {
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
    let reads = core::cell::Cell::new(0);
    let branch = |state: &mut ShardState, name: &str, from: &str| {
      let (reply, read) = crate::vcs_store::hg_tags::dispatch_blocking(
        state,
        &owner(),
        RequestBody::CreateBranch {
          repository: "hg".to_owned(),
          name: name.to_owned(),
          from: from.to_owned(),
          version: None,
        },
      );
      reads.set(reads.get() + u32::from(read));
      reply
    };
    pushed(state, hg, &made, &[0]);
    let mark = branch(state, "mark", "default");
    assert!(matches!(mark, ReplyBody::GreenCreated { .. }), "{mark:?}");
    assert_eq!(reads.get(), 0, "no head named a .hgtags revision yet");
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
    // `default` and `v1` reach the tags, read from the changeset store before each runs (A-767 C1); `tip` never.
    assert_eq!(reads.get(), 2, "the tags read for `default` and `v1`");
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
  crate::test_store::audit_on_stored_shard("hg", |state| {
    let imported_id = created(state, "imported", VcsTool::Mercurial);
    let (host, directory) = laid_out(FILE_INDEX.iter().copied());
    assert_eq!(
      begin(state, imported_id, (Box::new(host), directory)),
      Ok(())
    );
    imported(state, imported_id);
    let pushed_id = created(state, "pushed", VcsTool::Mercurial);
    let made = crate::repository::hg_push::tests::history_of(&[
      ("base", "a", b"a\n", None, "default"),
      ("second", "b", b"b\n", Some(0), "default"),
      ("third", "c", b"c\n", Some(1), "default"),
    ]);
    pushed(state, pushed_id, &made, &[0, 1, 2]);
    assert_eq!(
      (streams_of(state, imported_id), streams_of(state, pushed_id)),
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
  crate::test_store::audit_on_stored_shard("hg", |state| {
    let hg = created(state, "hg", VcsTool::Mercurial);
    let made = crate::repository::hg_push::tests::history_of(&[
      ("base", "a", b"a\n", None, "default"),
      ("second", "b", b"b, a longer text\n", Some(0), "default"),
    ]);
    pushed(state, hg, &made, &[0, 1]);
    let texts: Vec<[u8; 32]> = [&b"a\n"[..], b"b, a longer text\n"]
      .iter()
      .map(|text| *blake3::hash(text).as_bytes())
      .collect();
    let disk = state.vcs_disk.clone().expect("a store");
    let ask = |most: u64| {
      let mut asked = AskedTexts {
        want: Some(slates_hg::render::Want::Texts {
          texts: texts.clone(),
          most,
        }),
        given: None,
      };
      assert!(crate::test_store::answer_wants_blocking(
        &disk,
        u128::from_be_bytes(hg.bytes),
        &mut asked
      ));
      asked.given.expect("an answer")
    };
    assert_eq!(
      ask(u64::MAX),
      Some(vec![b"a\n".to_vec(), b"b, a longer text\n".to_vec()])
    );
    let held = u64::try_from(b"a\n".len() + b"b, a longer text\n".len()).unwrap();
    assert_eq!(ask(held - 1), None, "refused past its room");
  });
}

/// What a revision's text must be, as its writer kept it: a changeset's or a manifest's text whole; a file revision's
/// by the identities its row names (its bytes' BLAKE3 and its metadata block's), since a landed file's text leaves the
/// content store once its writer lands it (goal 93 item 2b(b)) and its row's names are what the writer kept.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kept {
  /// The text.
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
) -> Vec<(Vec<u8>, slates_hg::Node, Kept)> {
  use slates_hg::push::intake::{CHANGELOG_PATH, MANIFEST_PATH};
  use slates_hg::push::verify::Held;
  let mut room = Admission::new(ROOM);
  crate::vcs_store::lent_state(state, |state| {
    super::with_view(state, id, &mut room, |view, held, room| {
      let mut revisions = Vec::new();
      for rev in 0..view.count() {
        let node = view.node(rev).unwrap();
        let text = view
          .changeset_text(rev, room)
          .unwrap()
          .unwrap()
          .into_owned();
        revisions.push((CHANGELOG_PATH.to_vec(), node, Kept::Text(text)));
      }
      let mut position = 0;
      while let Some(linked) = view.manifest_linked(position) {
        let logged = view.manifest(&linked.node, room).unwrap().unwrap();
        revisions.push((
          MANIFEST_PATH.to_vec(),
          linked.node,
          Kept::Text(logged.text.into_owned()),
        ));
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

/// What the node's store keeps of `revisions` in repository `id`: how many it keeps as an edit against a base, and each
/// text made whole from its chain (`StoreHandle::texts_by_tool`, then `hg_view::texts_made`).
fn kept_by_the_store(
  state: &mut ShardState,
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
  let disk = state.vcs_disk.clone().expect("a store");
  let repository = u128::from_be_bytes(id.bytes);
  let logs = revisions
    .iter()
    .map(|(path, node, _)| (path.clone(), vec![*node]))
    .collect();
  let mut rows = Rows {
    want: Some(slates_hg::render::Want::Rows { logs }),
    rows: Vec::new(),
  };
  assert!(crate::test_store::answer_wants_blocking(
    &disk, repository, &mut rows
  ));
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
  assert!(crate::test_store::answer_wants_blocking(
    &disk, repository, &mut asked
  ));
  (
    edits,
    asked.given.expect("an answer").expect("within the room"),
  )
}

/// The changegroup a clone of repository `id` sends over the HTTP edge, its file revisions as the store keeps them
/// (A-767 C1 step 1a), as a push's part.
fn cloned_changegroup(
  state: &mut ShardState,
  id: RepositoryId,
) -> slates_hg::push::ChangegroupPart {
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
  let heads: Vec<String> = lend_in(state, id, &mut Admission::new(ROOM), |view, room| {
    slates_hg::serve::discovery::head_revs(view, room)
      .unwrap()
      .into_iter()
      .map(|rev| crate::verbs::hex_of(&view.node(rev).unwrap()))
      .collect()
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
  let disk = state.vcs_disk.clone().expect("the store");
  let repository = u128::from_be_bytes(id.bytes);
  let begun = lend_in(state, id, &mut Admission::new(ROOM), |view, left| {
    hg_http::begin(view, &request, 0, &mut Plain, left)
  })
  .expect("the answer begun");
  let Begun::Streamed(mut streamed) = begun else {
    panic!("a clone's answer is streamed");
  };
  let mut body = Vec::new();
  loop {
    assert!(crate::test_store::answer_wants_blocking(
      &disk,
      repository,
      &mut *streamed
    ));
    let more = lend_in(state, id, &mut Admission::new(ROOM), |view, left| {
      streamed.step(view, left, &mut body)
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
  crate::test_store::audit_on_stored_shard("hg", |state| {
    let formats = [
      ("dot", DOT),
      ("hybrid", HYBRID),
      ("legacy", LEGACY),
      ("v2", V2),
      ("fileindex", FILE_INDEX),
    ];
    for (name, files) in formats {
      let imported_id = created(state, &format!("{name}-imported"), VcsTool::Mercurial);
      let (host, directory) = laid_out(files.iter().copied());
      assert_eq!(
        begin(state, imported_id, (Box::new(host), directory)),
        Ok(())
      );
      imported(state, imported_id);
      let revisions = every_revision(state, imported_id);
      let made_as_kept = |made: &[Vec<u8>]| {
        made.len() == revisions.len()
          && made
            .iter()
            .zip(&revisions)
            .all(|(made, (_, _, kept))| kept.is(made))
      };
      let (imported_edits, made) = kept_by_the_store(state, imported_id, &revisions);
      assert!(made_as_kept(&made), "{name}: the import's texts made whole");
      let changegroup = cloned_changegroup(state, imported_id);
      let pushed_id = created(state, &format!("{name}-pushed"), VcsTool::Mercurial);
      push_part(state, pushed_id, changegroup);
      assert_eq!(
        every_revision(state, pushed_id),
        revisions,
        "{name}: the push holds the import's history"
      );
      let (pushed_edits, made) = kept_by_the_store(state, pushed_id, &revisions);
      assert!(made_as_kept(&made), "{name}: the push's texts made whole");
      assert!(
        imported_edits > 0 && pushed_edits > 0,
        "{name}: edits kept, {imported_edits} imported and {pushed_edits} pushed"
      );
    }
  });
}

/// Lands `changegroup` on repository `id` as a push of its owner lands it.
fn push_part(
  state: &mut ShardState,
  id: RepositoryId,
  changegroup: slates_hg::push::ChangegroupPart,
) {
  use slates_hg::push::{Asks, Taken, Unbundle};
  let unbundle = Unbundle {
    parts: vec![Taken {
      id: 0,
      mandatory: true,
      asks: Asks::Changegroup(changegroup),
    }],
  };
  let mut room = Admission::new(u64::MAX);
  let terms = (false, [7; 32], u64::MAX);
  let prepared = crate::repository::hg_push::prepare_served(state, id, &unbundle, terms, &mut room)
    .expect("the push prepared");
  let landed = crate::repository::hg_push::land_stored(state, (&owner(), id), prepared);
  assert!(
    landed.answer.is_ok() && landed.released.is_ok(),
    "{landed:?}"
  );
}
