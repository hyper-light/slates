//! A stock git clone's own requests, served (A-640 part 2): `git clone` at protocol v2 of small.git, whose requests
//! A-595 recorded on its own wire (`fixtures/http`, `make-http-fixtures.sh`, exchanges 04 and 05), answered from
//! small.git rebuilt in A-614's store. The rebuild is held to the clone's own wants, so it is the repository git asked
//! of.

use std::collections::BTreeSet;

use slates_vcs::ContentStore;

use super::{WholeObjects, pack_of, read_fetch, serving};
use crate::Admission;
use crate::http::Version;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::served;
use crate::store::{NamedRef, Repository};
use crate::unpack::{self, NoIndex};
use crate::upload::{Asked, Written, serve};
use crate::v2::Line;
use crate::v2::fetch::{Argument, Event, Section};
use crate::v2::ls_refs::{self, Attribute, Attributes, Ref, Target};
use crate::v2::request::{self, Arguments};

/// Format: the clone's `ls-refs` request, as git 2.50.1's client sent it (`peel`, `symrefs`, `unborn`, and prefixes
/// `refs/heads/`, `refs/tags/` and `HEAD`).
const LS_REFS: &[u8] =
  include_bytes!("../../../fixtures/http/04-post-git-upload-pack.request-body.bin");

/// Format: the clone's `fetch` request, as git 2.50.1's client sent it, inflated from the gzip it was sent in.
pub(super) const FETCH: &[u8] =
  include_bytes!("../../../fixtures/http/05-post-git-upload-pack.request-inflated.bin");

/// Format: the identity fast-import gave every commit, its author taken from its committer.
const SIGNATURE: &str = "test <test@example.com> 1112911993 +0000";

/// Format: the empty tree's name, which every small.git commit names.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// small.git's branches in name order, each with its commit's message: `main`, then `side0001` to `side0030`.
pub(super) fn branches() -> Vec<(String, String)> {
  let mut branches = vec![("refs/heads/main".to_string(), "main\n".to_string())];
  branches.extend((1..=30).map(|n| {
    (
      format!("refs/heads/side{n:04}"),
      format!("commit side{n:04}\n"),
    )
  }));
  branches
}

/// small.git rebuilt: the empty tree and a commit on it for each branch, in a store whose greens are the branches, in
/// order; and each branch's commit.
pub(super) fn small() -> (served::Served, Vec<ObjectId>) {
  let commits: Vec<Vec<u8>> = branches()
    .iter()
    .map(|(_, message)| {
      format!("tree {EMPTY_TREE}\nauthor {SIGNATURE}\ncommitter {SIGNATURE}\n\n{message}")
        .into_bytes()
    })
    .collect();
  let mut entries: Vec<(Kind, &[u8])> = vec![(Kind::Tree, b"")];
  entries.extend(
    commits
      .iter()
      .map(|commit| (Kind::Commit, commit.as_slice())),
  );
  let pack = pack_of(&entries);
  let objects = unpack::read_whole(
    &pack,
    ObjectFormat::Sha1,
    &NoIndex,
    &mut Admission::new(u64::MAX),
  )
  .unwrap()
  .objects;
  let tips: Vec<ObjectId> = commits
    .iter()
    .map(|commit| {
      objects
        .iter()
        .find(|object| object.content == *commit)
        .unwrap()
        .id
    })
    .collect();
  let hexes: Vec<String> = tips.iter().map(ToString::to_string).collect();
  let moves: Vec<(u64, &str)> = hexes
    .iter()
    .enumerate()
    .map(|(green, hex)| (u64::try_from(green).unwrap() + 1, hex.as_str()))
    .collect();
  let mut store = ContentStore::new(1 << 26, 1 << 10).unwrap();
  let mut repository = Repository::new(ObjectFormat::Sha1);
  served::commit(&mut repository, &mut store, &objects, &moves);
  let fixture = served::Served {
    repository,
    store,
    objects,
  };
  (fixture, tips)
}

/// The refs small.git serves: each branch, its green its place in order.
pub(super) fn refs(names: &[(String, String)]) -> Vec<NamedRef<'_>> {
  names
    .iter()
    .enumerate()
    .map(|(green, (name, _))| NamedRef {
      name: name.as_bytes(),
      green: u64::try_from(green).unwrap() + 1,
    })
    .collect()
}

/// What the clone's `request` is answered with by small.git, written in one step, and whether it was refused.
fn answered(request: &[u8]) -> (Vec<u8>, served::Served, bool) {
  let (fixture, _) = small();
  let names = branches();
  let refs = refs(&names);
  let served = serving(&fixture, &refs, b"refs/heads/main");
  let mut source = WholeObjects::new(&fixture.objects);
  let mut admission = Admission::new(u64::MAX);
  let mut answer = serve(
    &served,
    Version::V2,
    Asked::Request(request),
    &mut source,
    &mut admission,
  );
  let mut bytes = Vec::new();
  assert_eq!(
    answer.write(&served, &mut source, &mut admission, &mut bytes, usize::MAX),
    Written::End
  );
  let refused = answer.refusal().is_some();
  (bytes, fixture, refused)
}

/// A-640 part 2 (the rebuild is the repository git asked of): do read the clone's `fetch` request. Expect: its wants,
/// once each, exactly small.git's 31 branch tips as rebuilt here, and the one it sent twice (`HEAD`'s and `main`'s)
/// `main`'s.
#[test]
fn the_clones_wants_are_the_rebuilt_repositorys_tips() {
  let (_, tips) = small();
  let request::Request::Command(command) =
    request::decode(FETCH, &mut Admission::new(u64::MAX)).unwrap()
  else {
    panic!("not a command");
  };
  let Arguments::Fetch(arguments) = command.arguments else {
    panic!("not a fetch");
  };
  let sent: Vec<ObjectId> = arguments
    .iter()
    .filter_map(|line| match line.value {
      Argument::Want(id) => Some(id),
      _ => None,
    })
    .collect();
  let twice: Vec<&ObjectId> = sent
    .iter()
    .filter(|id| sent.iter().filter(|other| other == id).count() == 2)
    .collect();
  assert_eq!(twice, [&tips[0], &tips[0]]);
  let wants: BTreeSet<ObjectId> = sent.into_iter().collect();
  assert_eq!(wants, tips.into_iter().collect::<BTreeSet<_>>());
}

/// A-640 part 2 (a stock clone's `ls-refs`, its capabilities and flags without an LF as git's client writes them): do
/// serve it. Expect: `HEAD` with its target, then every branch in name order, each line what git's `ls-refs` writes
/// for small.git (no tag, so nothing peeled).
#[test]
fn a_stock_clones_ls_refs_lists_every_branch_head_first() {
  let (bytes, _, refused) = answered(LS_REFS);
  assert!(!refused);
  let (_, tips) = small();
  let names = branches();
  let head = Ref {
    target: Target::Id(tips[0]),
    name: b"HEAD",
    attributes: Attributes::default()
      .with(Attribute::SymrefTarget(b"refs/heads/main"))
      .unwrap(),
  };
  let mut lines = vec![Line::lf(head)];
  lines.extend(names.iter().zip(&tips).map(|((name, _), tip)| {
    Line::lf(Ref {
      target: Target::Id(*tip),
      name: name.as_bytes(),
      attributes: Attributes::default(),
    })
  }));
  let mut expected = Vec::new();
  ls_refs::encode_answer(&lines, ObjectFormat::Sha1, &mut expected).unwrap();
  assert_eq!(bytes, expected);
}

/// A-640 part 2 (a stock clone's `fetch`: 32 wants, one twice as `HEAD`'s and `main`'s, `thin-pack`, `no-progress`,
/// `ofs-delta` and `done`): do serve it. Expect: the packfile section alone, holding exactly small.git's 32 objects.
#[test]
fn a_stock_clones_fetch_is_answered_with_every_object() {
  let (bytes, fixture, refused) = answered(FETCH);
  assert!(!refused);
  let (events, names) = read_fetch(&bytes, false, &fixture.objects);
  assert_eq!(
    events,
    [Event::Header(Line::lf(Section::Packfile)), Event::Flush]
  );
  let every: BTreeSet<ObjectId> = fixture.objects.iter().map(|object| object.id).collect();
  assert_eq!(every.len(), 32);
  assert_eq!(names, every);
}

/// small.git's clone `fetch` answered in turns within `admission`, held past its room ([`crate::upload::Answer::holding`])
/// and passing over `skip` bytes the client has: the bytes written until it ends or is held, and the answer.
fn answered_holding(
  served: &crate::upload::Served<'_>,
  fixture: &served::Served,
  admission: &mut Admission,
  skip: usize,
) -> (Vec<u8>, crate::upload::Answer<super::WholePack>) {
  let mut source = WholeObjects::new(&fixture.objects);
  let mut answer = crate::upload::serve_in_turns(
    served,
    Version::V2,
    Asked::Request(FETCH),
    &mut source,
    admission,
  )
  .holding()
  .skip_written(skip);
  let mut bytes = Vec::new();
  while answer.held().is_none()
    && answer.write_within(
      served,
      &mut source,
      admission,
      &mut bytes,
      usize::MAX,
      &mut || true,
    ) == Written::More
  {}
  (bytes, answer)
}

/// The widest room in which small.git's clone `fetch` is held past its room (the last stage to run past it): the room,
/// what the held answer wrote, and the answer.
fn widest_held(
  served: &crate::upload::Served<'_>,
  fixture: &served::Served,
  whole: usize,
) -> (u64, Vec<u8>, crate::upload::Answer<super::WholePack>) {
  (0..u64::try_from(whole).unwrap().saturating_mul(4))
    .map(|room| {
      let (bytes, answer) = answered_holding(served, fixture, &mut Admission::new(room), 0);
      (room, bytes, answer)
    })
    .take_while(|(_, _, answer)| answer.held().is_some())
    .last()
    .expect("a room past which the clone is held")
}

/// What small.git's clone `fetch` writes within `room`, its refusal answered as it is with no hold, or held then given
/// up when `held`: the bytes, and the refusal.
fn written_within(
  served: &crate::upload::Served<'_>,
  fixture: &served::Served,
  room: u64,
  held: bool,
) -> (Vec<u8>, Option<crate::upload::Refusal>) {
  let mut source = WholeObjects::new(&fixture.objects);
  let mut admission = Admission::new(room);
  let mut answer = crate::upload::serve_in_turns(
    served,
    Version::V2,
    Asked::Request(FETCH),
    &mut source,
    &mut admission,
  );
  if held {
    answer = answer.holding();
  }
  let mut bytes = Vec::new();
  loop {
    let written = answer.write_within(
      served,
      &mut source,
      &mut admission,
      &mut bytes,
      usize::MAX,
      &mut || true,
    );
    if answer.held().is_some() {
      answer.give_up();
      admission = Admission::new(u64::MAX);
      continue;
    }
    if written == Written::End {
      break;
    }
  }
  (bytes, answer.refusal().cloned())
}

/// Whether `bytes` hold git's band-3 words for a pack that could not be finished.
fn aborts(bytes: &[u8]) -> bool {
  bytes
    .windows(crate::upload::ABORT.len())
    .any(|window| window == crate::upload::ABORT)
}

/// small.git's clone `fetch`, answered whole passing over each count of its first bytes through its `packfile` section's
/// header, writes `whole`'s bytes after that count.
fn assert_skips_each_count(
  served: &crate::upload::Served<'_>,
  fixture: &served::Served,
  whole: &[u8],
) {
  let header: &[u8] = b"000dpackfile\n";
  assert!(whole.starts_with(header));
  for skip in 0..=header.len() {
    let (rest, _) = answered_holding(served, fixture, &mut Admission::new(u64::MAX), skip);
    assert_eq!(
      rest.as_slice(),
      whole.get(skip..).unwrap(),
      "passing over {skip} bytes"
    );
  }
}

/// A-855 (main's ruling of 2026-11-21: a refusal costs time, never an answer; git/git's clone over a request's room was
/// answered refused `PastAdmission` and its pack cut with git's abort words). Do: answer small.git's stock clone `fetch`
/// in turns within each room from none up, holding a refusal past its room, the widest room that holds it taken; answer
/// it again within an unbounded room passing over what the held one wrote; answer it whole passing over each count of
/// its first bytes through its `packfile` section's header; then hold it once more and give it up. Expect: the held
/// answer refused nothing yet and wrote no abort; what it wrote and what the answer made again wrote together are the
/// whole answer byte for byte; an answer passing over `k` bytes writes the whole answer's bytes after its `k`th; given
/// up, the answer refused and written as it was before the change.
#[test]
fn a_clone_held_past_its_room_and_answered_again_in_a_wider_one_is_the_whole_answer() {
  let (fixture, _) = small();
  let names = branches();
  let refs = refs(&names);
  let served = serving(&fixture, &refs, b"refs/heads/main");
  let (whole, answer) = answered_holding(&served, &fixture, &mut Admission::new(u64::MAX), 0);
  assert!(answer.refusal().is_none() && answer.held().is_none());
  let (room, bytes, answer) = widest_held(&served, &fixture, whole.len());
  assert!(answer.refusal().is_none(), "held, not refused");
  assert!(!aborts(&bytes), "no abort written while held");
  let (rest, again) = answered_holding(
    &served,
    &fixture,
    &mut Admission::new(u64::MAX),
    answer.written(),
  );
  assert!(again.refusal().is_none() && again.held().is_none());
  assert_eq!(
    [bytes, rest].concat(),
    whole,
    "answered whole, byte for byte"
  );
  assert_skips_each_count(&served, &fixture, &whole);
  let given_up = written_within(&served, &fixture, room, true);
  assert!(given_up.1.is_some(), "given up: refused");
  assert_eq!(
    given_up,
    written_within(&served, &fixture, room, false),
    "given up: answered as before the change"
  );
}
