//! A-682's tests: the daemon's lender over A-662's map, driven as upload-pack's edges drive it, each on a real
//! one-shard daemon whose repository took its objects through A-635's append: a pushed branch listed and fetched whole,
//! a ref the reader may not read neither listed nor wanted, a name that is no git ref not lent, a store not held here
//! not lent, and an SSH session the SSH site's services start over the daemon's lender.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use slates_db::catalog::{Principal, RepositoryId, RepositoryRecord, VolumeId as DbVolumeId};
use slates_git::Admission;
use slates_git::http::Version;
use slates_git::object_name::{ObjectFormat, ObjectId};
use slates_git::pktline::{self, MAX_LEN, Packet};
use slates_git::unpack::{self, NoIndex};
use slates_git::upload::{self, Answer, Asked, Refusal, Streamed, Written};
use slates_ipc::protocol::{
  PolicyEffect, PolicyEntry, PolicyNames, PolicyRight, PolicyRule, PolicyWho, ReplyBody,
  RequestBody, VcsTool,
};

use super::super::Target;
use super::super::hg_view::ServedViews;
use super::super::tests::{
  Brought, FIRST_DATE, appended, begun, commit_of, owner, push_through, repository, tree_of,
};
use super::{ShardStores, lend_on};
use crate::repository::push::BranchChange;
use crate::state::ShardState;
use crate::vcs_http::git::ThisShard;
use crate::vcs_ssh::site::{Command, Next, Service, Services, Start, ToolServices};

/// Shape: the room a request and its answer are admitted: far past the tests' few small objects.
const ROOM: u64 = 1 << 20;
/// Shape: the daemon's liveness intervals a drive (an answer's steps, an encoding's turns, an SSH command's steps) may go
/// without progress before it is judged never to end ([`Judge`]): far past any wait between two of its signs of
/// progress on a loaded host. Paced work's steps are a turn's quantum each and at least one unit, so how many a drive
/// takes is the host's load, never a bound (main 01a20182's order of 2026-11-22 05:47:
/// `t_goal93_a_clone_answered_before_its_reach_sets_are_derived_is_whole`, at load ~35, spent a fixed 4,096 steps with
/// 131,053 of its 164,196 bytes written, every step progressing; its siblings by the order of 06:13).
const STALLED_INTERVALS: u64 = 60;
/// Shape: the uid of a reader the repository's owner is not.
const READER: u32 = 4321;
/// Shape: how long the test waits for the SSH session's task: four liveness windows, past every step it takes.
const PATIENCE: Duration = Duration::from_nanos(4 * crate::daemon::LIVENESS_BUDGET_NS);
/// Shape: the SSH session's `GIT_PROTOCOL`, protocol v2.
const PROTOCOL_V2: &[u8] = b"version=2";
/// Shape: a flush alone, with which a client ends its session.
const FLUSH: &[u8] = b"0000";

/// The reader, as the database names it.
fn reader() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: READER,
  }
}

/// `data` as one pkt-line: its length in four hexadecimal digits, the four included, then the data.
fn packet(data: &[u8]) -> Vec<u8> {
  let mut line = format!("{:04x}", data.len() + 4).into_bytes();
  line.extend_from_slice(data);
  line
}

/// A v2 command request: `command=<command>`, the delimiter, each of `arguments` a line, then the flush.
pub(crate) fn request(command: &str, arguments: &[String]) -> Vec<u8> {
  let mut bytes = packet(format!("command={command}\n").as_bytes());
  bytes.extend_from_slice(b"0001");
  for argument in arguments {
    bytes.extend(packet(format!("{argument}\n").as_bytes()));
  }
  bytes.extend_from_slice(FLUSH);
  bytes
}

/// An `ls-refs` request for `HEAD` and the branches, each symref's target named.
pub(crate) fn ls_refs() -> Vec<u8> {
  let arguments = ["symrefs", "ref-prefix HEAD", "ref-prefix refs/heads/"];
  request("ls-refs", &arguments.map(str::to_owned))
}

/// A `fetch` request: `wants` by id, `refs` by name, and `done`, with no progress.
pub(crate) fn fetch(wants: &[ObjectId], refs: &[&str]) -> Vec<u8> {
  let mut arguments: Vec<String> = wants.iter().map(|id| format!("want {id}")).collect();
  arguments.extend(refs.iter().map(|name| format!("want-ref {name}")));
  arguments.extend(["no-progress".to_owned(), "done".to_owned()]);
  request("fetch", &arguments)
}

/// What `reader` is answered for `asked` of repository `id` on the shard, as an edge serves it: the request's step lent
/// the refs `reader` may read, then each step of the answer lent the objects alone, one pkt-line's room a step. The
/// answer's bytes and why it was refused, if it was.
pub(crate) fn answered(
  state: &mut ShardState,
  (id, reader): (RepositoryId, &Principal),
  asked: Asked<'_>,
) -> (Vec<u8>, Option<Refusal>) {
  let mut admission = Admission::new(ROOM);
  let mut answer = lend_on(state, (id, Some(reader)), |served, packs| {
    upload::serve(served, Version::V2, asked, packs, &mut admission)
  })
  .expect("the request's step lent");
  let out = written_to_end(state, id, &mut answer, &mut admission);
  (out, answer.refusal().cloned())
}

/// `answer`'s steps on the shard, each lent the objects alone, one pkt-line's room a step, until it ends: its bytes. Its
/// steps are paced, a turn's quantum each and at least one unit of its pack (`Fragments`, A-718), so how many it takes is
/// the host's load and not the answer's: it is judged never to end only once [`STALLED_INTERVALS`] liveness intervals
/// pass with neither a byte written nor its pack's counters moved ([`answer_seen`], its turns aside), and the failure
/// says what it saw ([`Judge`]).
fn written_to_end(
  state: &mut ShardState,
  id: RepositoryId,
  answer: &mut Answer<Streamed>,
  admission: &mut Admission,
) -> Vec<u8> {
  let mut out = Vec::new();
  let mut judge = Judge::new("an answer", answer_seen(answer, &out));
  loop {
    let written = lend_on(state, (id, None), |served, packs| {
      answer.write(served, packs, admission, &mut out, MAX_LEN)
    })
    .expect("the answer's step lent");
    if written == Written::End {
      return out;
    }
    if let Err(stalled) = judge.stepped(answer_seen(answer, &out)) {
      panic!("{stalled}: (bytes written, its pack's counters)")
    }
  }
}

/// What an answer shows of its progress, `out` its bytes so far: their count, and its pack's counters, its turns aside
/// (a turn is taken whether or not it progressed).
fn answer_seen(
  answer: &Answer<Streamed>,
  out: &[u8],
) -> (usize, Option<slates_git::store::UploadCounters>) {
  let counters = answer
    .pack_source()
    .map(|pack| slates_git::store::UploadCounters {
      turns: 0,
      ..pack.counters()
    });
  (out.len(), counters)
}

/// A drive's judge ([`STALLED_INTERVALS`]): what the drive last showed of its progress (`P`, compared step to step), when
/// it last changed, and the steps taken.
struct Judge<P> {
  /// What is driven, as its failure names it.
  what: &'static str,
  /// What it showed after its last step.
  last: P,
  /// When that last changed.
  progressed: Instant,
  /// The steps taken.
  steps: u64,
}

impl<P: PartialEq + core::fmt::Debug> Judge<P> {
  /// A judge of `what`, which shows `seen` before its first step.
  fn new(what: &'static str, seen: P) -> Judge<P> {
    Judge {
      what,
      last: seen,
      progressed: Instant::now(),
      steps: 0,
    }
  }

  /// After a step that did not end the drive, showing `seen`: its words once [`STALLED_INTERVALS`] liveness intervals
  /// passed with nothing it shows changed, naming its steps and what it last showed.
  fn stepped(&mut self, seen: P) -> Result<(), String> {
    self.steps += 1;
    if seen != self.last {
      self.last = seen;
      self.progressed = Instant::now();
      return Ok(());
    }
    let window =
      Duration::from_nanos(crate::daemon::LIVENESS_BUDGET_NS.saturating_mul(STALLED_INTERVALS));
    let since = self.progressed.elapsed();
    if since < window {
      return Ok(());
    }
    Err(format!(
      "{} that never ended: no progress for {since:?} (the window, {STALLED_INTERVALS} liveness intervals), after {} \
       steps, last showing {:?}",
      self.what, self.steps, self.last
    ))
  }
}

/// [`answered`] for a `fetch`, with what the pack's stream did: its bytes, and its counters (A-718).
pub(crate) fn fetched_with_counters(
  state: &mut ShardState,
  (id, reader): (RepositoryId, &Principal),
  wants: &[u8],
) -> (Vec<u8>, slates_git::store::UploadCounters) {
  let mut admission = Admission::new(ROOM);
  let mut answer = lend_on(state, (id, Some(reader)), |served, packs| {
    upload::serve(
      served,
      Version::V2,
      Asked::Request(wants),
      packs,
      &mut admission,
    )
  })
  .expect("the request's step lent");
  let out = written_to_end(state, id, &mut answer, &mut admission);
  let counters = answer.pack_source().expect("a fetch's pack").counters();
  (out, counters)
}

/// Turns repository `id`'s encoding until it is idle, as a fetch's request does before its answer (A-718): the turns
/// taken. Judged on its progress ([`Judge`]): the units its encoding under way has done, the versions derived and
/// encoded.
fn encoded(state: &mut ShardState, id: RepositoryId) -> usize {
  use super::super::{git_derive, git_encode};
  let seen = |state: &ShardState| {
    let counted = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
    (
      git_encode::units_done(state, id),
      counted(git_derive::DERIVED),
      counted(git_encode::ENCODED),
    )
  };
  let mut judge = Judge::new("an encoding", seen(state));
  let mut turns = 0;
  while git_encode::turn(state, id) != git_encode::Turned::Idle {
    turns += 1;
    if let Err(stalled) = judge.stepped(seen(state)) {
      panic!("{stalled}: (its units, versions derived, versions encoded)")
    }
  }
  turns
}

/// The data packets of `bytes` up to the first flush, each whole; a stream that ends before one fails the test.
pub(crate) fn lines_of(bytes: &[u8]) -> Vec<Vec<u8>> {
  let mut lines = Vec::new();
  let mut at = 0;
  loop {
    let rest = bytes.get(at..).unwrap_or_default();
    match pktline::read(rest, u64::try_from(at).unwrap_or(u64::MAX)) {
      Ok(Some((Packet::Data(data), length))) => {
        lines.push(data.to_vec());
        at += length;
      }
      Ok(Some((Packet::Flush, _))) => return lines,
      other => panic!("no flush ended the lines ({other:?}): {bytes:?}"),
    }
  }
}

/// The pack a `fetch` answer carries: its `packfile` section's band-1 data, joined; progress on band 2 dropped.
pub(crate) fn pack_of(answer: &[u8]) -> Vec<u8> {
  let lines = lines_of(answer);
  let (section, packets) = lines.split_first().expect("a section");
  assert_eq!(section.as_slice(), b"packfile\n", "{answer:?}");
  let mut pack = Vec::new();
  for line in packets {
    match line.split_first() {
      Some((1, data)) => pack.extend_from_slice(data),
      Some((2, _)) => {}
      other => panic!("not the pack's band: {other:?}"),
    }
  }
  pack
}

/// The ids of the objects `pack` holds, read whole as git's index-pack reads one, in name order.
fn ids_in(pack: &[u8]) -> Vec<String> {
  let unpacked = unpack::read_whole(
    pack,
    ObjectFormat::Sha1,
    &NoIndex,
    &mut Admission::new(ROOM),
  )
  .expect("a whole pack");
  let mut ids: Vec<String> = unpacked
    .objects
    .iter()
    .map(|object| object.id.to_string())
    .collect();
  ids.sort();
  ids
}

/// The ids of the objects `pack` holds, in the order it sends them.
fn sent_in_order(pack: &[u8]) -> Vec<String> {
  let unpacked = unpack::read_whole(
    pack,
    ObjectFormat::Sha1,
    &NoIndex,
    &mut Admission::new(ROOM),
  )
  .expect("a whole pack");
  unpacked
    .objects
    .iter()
    .map(|object| object.id.to_string())
    .collect()
}

/// The ids of `objects`, in name order.
fn ids_of(objects: &[Brought]) -> Vec<String> {
  let mut ids: Vec<String> = objects.iter().map(|(_, id, _)| id.to_string()).collect();
  ids.sort();
  ids
}

/// `ls-refs`' line for `name` at `head`, its symref's target `target` when it has one.
pub(crate) fn ref_line(head: ObjectId, name: &str, target: Option<&str>) -> Vec<u8> {
  match target {
    Some(target) => format!("{head} {name} symref-target:{target}\n").into_bytes(),
    None => format!("{head} {name}\n").into_bytes(),
  }
}

/// Makes branch `name` of `repository` at `version` of `from`, whose commit is `head`, from a push of `objects` that
/// an earlier push committed: the branch's store move with no object, as a push of several refs appends each.
fn branch_at(
  state: &mut ShardState,
  (repository, objects): (RepositoryId, &[Brought]),
  (name, from, version): (&str, DbVolumeId, u64),
  head: ObjectId,
) {
  let mut sealed = begun(state, repository, objects)
    .seal(state)
    .expect("sealed");
  let changes = vec![BranchChange::Create {
    name: name.to_owned(),
    from,
    version,
    work: None,
  }];
  let moves = [(Target::Created(name.to_owned()), Some(head))];
  appended(state, repository, changes, (&mut sealed, &moves)).expect("the branch");
  sealed.abandon(state);
}

/// A git repository `name` on the shard whose default branch `main` holds one commit, and each of `branches` made at
/// that commit: its id, the commit's objects and the commit.
fn pushed_repository(
  state: &mut ShardState,
  name: &str,
  branches: &[&str],
) -> (RepositoryId, Vec<Brought>, ObjectId) {
  let (id, root) = repository(state, name, VcsTool::Git);
  let files = [(b"README".to_vec(), b"one\n".to_vec())];
  let (objects, head) = commit_of(&files, &[], FIRST_DATE);
  let version = push_through(state, (id, root), &objects, (None, head, tree_of(&objects)));
  for branch in branches {
    branch_at(state, (id, &objects), (branch, root, version), head);
  }
  (id, objects, head)
}

/// Sets repository `name`'s policy to its one rule: `READER` may read `refs/heads/main`.
fn reader_reads_main(state: &mut ShardState, name: &str) {
  let rule = PolicyRule {
    names: PolicyNames::Refs,
    pattern: "refs/heads/main".to_owned(),
    right: PolicyRight::Read,
    effect: PolicyEffect::Allow,
    who: PolicyWho::AnyOf {
      entries: vec![PolicyEntry::Is {
        principal: slates_ipc::protocol::Principal::Uid {
          host: 0,
          uid: READER,
        },
      }],
    },
  };
  let reply = crate::verbs::dispatch(
    state,
    1,
    &owner(),
    RequestBody::SetRepositoryPolicy {
      repository: name.to_owned(),
      read: 0,
      rules: vec![rule],
      hooks: Vec::new(),
      hooks_env: Vec::new(),
      signers: Vec::new(),
      hg_publishing: true,
    },
  );
  assert_eq!(reply, ReplyBody::Acknowledged);
}

/// A-682. Do: push one commit to a git repository's default branch `main` through A-635's append on the owner shard;
/// then, as its owner, ask through the lender for `ls-refs` (symrefs, `HEAD` and the branches) and `fetch` the commit
/// with `done`. Expect: `HEAD` listed at the commit naming `refs/heads/main`, then `refs/heads/main` at it, then the
/// flush; the fetch's packfile section a whole pack, read as index-pack reads one, holding exactly the commit, its tree
/// and its blob; neither answer refused.
#[test]
fn t_a682_a_pushed_branch_is_listed_and_fetched_whole_from_the_store() {
  let (listed, fetched, objects, head) =
    crate::test_store::audit_on_stored_shard("vcs_store-upload", |state| {
      let (id, objects, head) = pushed_repository(state, "lent", &[]);
      let listed = answered(state, (id, &owner()), Asked::Request(&ls_refs()));
      let wants = fetch(&[head], &[]);
      let fetched = answered(state, (id, &owner()), Asked::Request(&wants));
      (listed, fetched, objects, head)
    });
  let expected = vec![
    ref_line(head, "HEAD", Some("refs/heads/main")),
    ref_line(head, "refs/heads/main", None),
  ];
  let observed = (lines_of(&listed.0), listed.1, fetched.1);
  assert_eq!(observed, (expected, None, None));
  assert_eq!(ids_in(&pack_of(&fetched.0)), ids_of(&objects));
}

/// A-724 piece 2d (§4.17.5 "Learned demand": a repository's working set as its miss-ratio curve). Do: push one commit
/// to each of two git repositories; fetch the first's commit twice through the lender, the second's never. Expect: the
/// first's usage row holds a curve that noted the fetches' reads, and its second fetch found them again: its miss ratio
/// at a cache of its last bin edge (all it read is under one window) below its ratio with no cache. The second's row
/// holds no curve: a step's reads are noted in its own repository's alone.
#[test]
fn t_a724_a_fetchs_reads_are_noted_in_its_own_repositorys_curve() {
  let (read, kept, cold, other) =
    crate::test_store::audit_on_stored_shard("vcs_store-upload", |state| {
      let (id, _, head) = pushed_repository(state, "curved", &[]);
      let (quiet, _, _) = pushed_repository(state, "quiet", &[]);
      let wants = fetch(&[head], &[]);
      for _ in 0..2 {
        let fetched = answered(state, (id, &owner()), Asked::Request(&wants));
        assert_eq!(fetched.1, None);
      }
      let row = state
        .vcs_usage
        .usage(&id)
        .expect("the fetched repository's row");
      let curve = row.curve().expect("its curve");
      let other = state
        .vcs_usage
        .usage(&quiet)
        .is_some_and(|row| row.curve().is_some());
      (
        curve.reads(),
        curve.miss_ratio(curve.edges().last().unwrap_or(0)),
        curve.miss_ratio(0),
        other,
      )
    });
  assert!(read > 0.0, "no read noted");
  assert!(kept < cold, "kept {kept} vs cold {cold}");
  assert!(!other, "a read noted in another repository's curve");
}

/// A-682 (§4.17.1 "Policy": a ref lent only to a reader its rule lets read it, a ref no rule speaks to its owner's
/// alone). Do: on a repository whose one rule lets `READER` read `refs/heads/main`, push a commit to `main` and make
/// `topic` at it; ask `ls-refs` as the owner and as the reader; then, as the reader, fetch `refs/heads/topic` by name.
/// Expect: the owner listed `HEAD`, `main` and `topic`; the reader `HEAD` and `main` alone; the reader's `want-ref`
/// refused as git refuses a ref it does not have ("unknown ref").
#[test]
fn t_a682_a_ref_the_reader_may_not_read_is_neither_listed_nor_wanted() {
  let (by_owner, by_reader, wanted, head) =
    crate::test_store::audit_on_stored_shard("vcs_store-upload", |state| {
      let (id, root) = repository(state, "hidden", VcsTool::Git);
      reader_reads_main(state, "hidden");
      let files = [(b"README".to_vec(), b"one\n".to_vec())];
      let (objects, head) = commit_of(&files, &[], FIRST_DATE);
      let version = push_through(state, (id, root), &objects, (None, head, tree_of(&objects)));
      branch_at(state, (id, &objects), ("topic", root, version), head);
      let ls_refs = ls_refs();
      let by_owner = answered(state, (id, &owner()), Asked::Request(&ls_refs));
      let by_reader = answered(state, (id, &reader()), Asked::Request(&ls_refs));
      let topic = fetch(&[], &["refs/heads/topic"]);
      let wanted = answered(state, (id, &reader()), Asked::Request(&topic));
      (by_owner, by_reader, wanted, head)
    });
  let main = vec![
    ref_line(head, "HEAD", Some("refs/heads/main")),
    ref_line(head, "refs/heads/main", None),
  ];
  let mut everything = main.clone();
  everything.push(ref_line(head, "refs/heads/topic", None));
  let unknown = Refusal::UnknownRef {
    name: b"refs/heads/topic".to_vec(),
  };
  let observed = (lines_of(&by_owner.0), lines_of(&by_reader.0), wanted.1);
  assert_eq!(observed, (everything, main, Some(unknown)));
}

/// A-682 (each register name one git ref by git's push's rule, A-654's `ref_of`). Do: push a commit to a repository's
/// `main`, and make at it the branch `my topic`, a name git's `check_refname_format` refuses, and the branch
/// `refs/heads/main`, a name whose ref is `main`'s; ask `ls-refs` as the owner. Expect: `HEAD` and `refs/heads/main`
/// listed once each and the answer whole: neither name is a git ref, so neither refuses the listing (a name protocol v2
/// cannot carry; one ref listed twice, which upload-pack's order check refuses).
#[test]
fn t_a682_a_name_that_is_no_git_ref_is_not_lent() {
  let (listed, head) = crate::test_store::audit_on_stored_shard("vcs_store-upload", |state| {
    let (id, _, head) = pushed_repository(state, "spaced", &["my topic", "refs/heads/main"]);
    let listed = answered(state, (id, &owner()), Asked::Request(&ls_refs()));
    (listed, head)
  });
  let expected = vec![
    ref_line(head, "HEAD", Some("refs/heads/main")),
    ref_line(head, "refs/heads/main", None),
  ];
  assert_eq!((lines_of(&listed.0), listed.1), (expected, None));
}

/// A-682. Do: on one shard, create a Mercurial repository, and a git repository whose default branch `my main` makes no
/// ref git can carry; ask the lender for each, and for a repository no record names, as the owner and for an answer's
/// next bytes. Expect: none lent: the Mercurial store is another tool's, the git repository has no `HEAD` git could name
/// (its store held, its objects still lent to an answer's later steps), and the unnamed one holds nothing here.
#[test]
fn t_a682_a_store_not_held_here_is_not_lent() {
  let lent = crate::test_store::audit_on_stored_shard("vcs", |state| {
    let (mercurial, _) = repository(state, "elsewhere", VcsTool::Mercurial);
    let reply = crate::test_store::create(
      state,
      &owner(),
      RequestBody::CreateRepository {
        name: "unnamed-head".to_owned(),
        tool: VcsTool::Git,
        format: slates_ipc::protocol::ObjectFormat::Sha1,
        default_branch: "my main".to_owned(),
        green: None,
      },
    );
    assert!(matches!(reply, ReplyBody::GreenCreated { .. }), "{reply:?}");
    let spaced = state
      .db
      .partition()
      .repositories()
      .repository_named("unnamed-head");
    let spaced = spaced.map(|record| record.id).expect("the record");
    let nobody = RepositoryId { bytes: [0xa6; 16] };
    let asked = |state: &mut ShardState, id| {
      let request = lend_on(state, (id, Some(&owner())), |_, _| ()).is_some();
      let objects = lend_on(state, (id, None), |_, _| ()).is_some();
      (request, objects)
    };
    [
      asked(state, mercurial),
      asked(state, spaced),
      asked(state, nobody),
    ]
  });
  assert_eq!(lent, [(false, false), (false, true), (false, false)]);
}

/// Steps `command` until it waits for input or exits, as the SSH edge does, taking all it writes: what it wrote and what
/// it waits for; a refusal's words and status instead, if it refused.
async fn run_command(command: &mut impl Command) -> Result<(Vec<u8>, Next), String> {
  let mut written = Vec::new();
  let mut judge = Judge::new("a command", 0usize);
  loop {
    let stepped = command
      .step(crate::vcs_ssh::site::ClientGone::never())
      .await
      .map_err(|refused| format!("{refused:?}"))?;
    let out = command.output().1.to_vec();
    command.sent(0, out.len());
    written.extend(out);
    if stepped.next != Next::More {
      return Ok((written, stepped.next));
    }
    judge
      .stepped(written.len())
      .map_err(|stalled| format!("{stalled} bytes: {written:?}"))?;
  }
}

/// One SSH session of git's upload-pack over `record`'s repository, as the SSH site's services start it over the
/// daemon's lenders (git's, and Mercurial's beside it, A-679) for its owner at protocol v2: its advertisement, then each
/// of `requests`' answers, each with what the command waits for after it; none when the services would not start it.
async fn ssh_session(
  record: &RepositoryRecord,
  requests: &[Vec<u8>],
) -> Result<Vec<(Vec<u8>, Next)>, String> {
  let mut services = ToolServices::new(ShardStores, ServedViews, ThisShard, ROOM);
  let start = Start {
    service: Service::GitUploadPack,
    repository: Some(record),
    principal: &owner(),
    git_protocol: Some(PROTOCOL_V2),
  };
  let mut command = services
    .start(&start)
    .map_err(|unserved| format!("{unserved:?}"))?;
  let mut answers = vec![run_command(&mut command).await?];
  for request in requests {
    command
      .input(request)
      .map_err(|refused| format!("{refused:?}"))?;
    answers.push(run_command(&mut command).await?);
  }
  Ok(answers)
}

/// A-682 (A-651 part 2's upload-pack over SSH, served from the store A-662 holds). Do: on a daemon's shard, push a
/// commit to a git repository's `main`; then, from a task on that shard, start git's upload-pack through the SSH site's
/// services over the daemon's lender (`ShardStores`) for the owner at protocol v2, and send `ls-refs`, a `fetch` of the
/// commit, and a flush alone. Expect: the advertisement first (`version 2`); `HEAD` and `main` listed at the commit; the
/// fetch's pack holding exactly the commit's objects; the flush ending the session with status 0. Before A-682 no lender
/// held a store, so the site's services never started it.
#[test]
fn t_a682_an_ssh_session_fetches_through_the_daemons_lender() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (record, objects, head) = crate::daemon::observe_first(&daemon, |state| {
    let (id, objects, head) = pushed_repository(state, "over-ssh", &[]);
    let record = state.db.partition().repositories().repository(id).cloned();
    (record.expect("the record"), objects, head)
  });
  let requests = vec![ls_refs(), fetch(&[head], &[]), FLUSH.to_vec()];
  let (sent, outcome) = mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |_state| {
    let task = slates_rt::futures::spawn(async move {
      let _ = sent.send(ssh_session(&record, &requests).await);
    })
    .expect("the session's task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let answers = outcome.recv_timeout(PATIENCE).expect("the session ended");
  daemon.stop();
  let answers = answers.expect("the session served");
  let [advertised, listed, fetched, ended] =
    <[(Vec<u8>, Next); 4]>::try_from(answers).expect("four answers");
  let expected = vec![
    ref_line(head, "HEAD", Some("refs/heads/main")),
    ref_line(head, "refs/heads/main", None),
  ];
  let observed = (
    advertised.0.starts_with(b"000eversion 2\n"),
    lines_of(&listed.0),
    ids_in(&pack_of(&fetched.0)),
    ended,
  );
  let expected = (
    true,
    expected,
    ids_of(&objects),
    (Vec::new(), Next::Exited(0)),
  );
  assert_eq!(observed, expected, "{advertised:?}");
}

/// A-718 (§4.17.2 "A push" item 8: a pushed version encoded once, off the answer's path, so a fetch copies its kept
/// fragments rather than searching its objects for deltas again). Do: push one commit to a git repository's `main`
/// through A-635's append on the owner shard and fetch it; then turn the repository's encoding until it is idle, turn it
/// again, and fetch the commit again. Expect: the first fetch sends its three objects from the stream's loose stage
/// (each sent on as its pushed entry lies, A-859, or encoded afresh) and copies no kept part;
/// the encoding keeps the version's fragments once (counted encoded once, and a second encoding idle at once, nothing
/// encoded again); the second fetch copies kept parts and encodes nothing afresh; both packs hold exactly the commit,
/// its tree and its blob, read as index-pack reads one.
#[test]
fn t_a718_a_pushed_version_is_encoded_once_and_fetched_from_its_kept_fragments() {
  let (before, after, encoded_count, again, objects) =
    crate::test_store::audit_on_stored_shard("vcs_store-upload", |state| {
      let (id, objects, head) = pushed_repository(state, "encoded", &[]);
      let wants = fetch(&[head], &[]);
      let before = fetched_with_counters(state, (id, &owner()), &wants);
      encoded(state, id);
      let again = encoded(state, id);
      let encoded_count = state
        .refusals
        .get(super::super::git_encode::ENCODED)
        .copied();
      let after = fetched_with_counters(state, (id, &owner()), &wants);
      (before, after, encoded_count, again, objects)
    });
  assert_eq!((encoded_count, again), (Some(1), 0));
  let fresh = |counters: &slates_git::store::UploadCounters| {
    counters.loose_whole + counters.loose_deltas + counters.reused_whole + counters.reused_deltas
  };
  assert_eq!(
    (before.1.parts_copied, fresh(&before.1) > 0),
    (0, true),
    "{:?}",
    before.1
  );
  assert_eq!(
    (after.1.parts_copied > 0, fresh(&after.1)),
    (true, 0),
    "{:?}",
    after.1
  );
  for (pack, _) in [before, after] {
    assert_eq!(ids_in(&pack_of(&pack)), ids_of(&objects));
  }
}

/// An annotated tag `name` of commit `target`, as git writes one: its object, its id and its bytes.
fn annotated_tag(
  target: ObjectId,
  name: &str,
) -> (slates_git::object_name::Kind, ObjectId, Vec<u8>) {
  use slates_git::object_name::{Kind, name_of};
  let bytes = format!(
    "object {target}\ntype commit\ntag {name}\ntagger A <a@example.com> 1700000000 +0000\n\n{name}\n"
  )
  .into_bytes();
  let id = name_of(ObjectFormat::Sha1, Kind::Tag, &bytes).expect("named");
  (Kind::Tag, id, bytes)
}

/// Pushes annotated tag `name` of commit `target` to repository `id` as the ref `refs/tags/<name>`, from `main`'s green
/// `root` at `version`.
fn tagged(
  state: &mut ShardState,
  (id, root, version): (RepositoryId, DbVolumeId, u64),
  target: ObjectId,
  name: &str,
) {
  let tag = annotated_tag(target, name);
  let tag_id = tag.1;
  branch_at(
    state,
    (id, &[tag]),
    (&format!("refs/tags/{name}"), root, version),
    tag_id,
  );
}

/// A-745 (§4.17.1 "A fetch's answer" rule 4: a fetch reads the index as it stood when it began, so a push landing
/// meanwhile neither stops it nor changes its answer). Do: push a commit to `main` and tag it `v1` (annotated); answer a
/// `fetch` of the commit with `include-tag` whole, the start generation's answer; then answer it again in turns through
/// the lender's hold, one unit a step, and after its first step push a second commit to `main` and tag the first commit
/// `v2`. Expect: the held answer the start generation's byte for byte (`v1`'s tag, not `v2`'s, nor the second commit;
/// until the encoder's level was chosen from measured rates, not timed on the shard's clock, two streams could deflate one
/// object at two levels, and this compared the objects alone); and, holding no generation and lent the objects alone as the
/// edges lent an answer's later steps before this, the same steps' pack other objects (its `include-tag` scan meets no
/// ref, so the tag is lost), so the hold is what keeps it.
#[test]
fn a745_a_fetch_answers_from_the_generation_it_began_on_whatever_lands_meanwhile() {
  let (start, held, unheld) =
    crate::test_store::audit_on_stored_shard("vcs_store-upload", |state| {
      let (id, root) = repository(state, "generations", VcsTool::Git);
      let files = [(b"README".to_vec(), b"one\n".to_vec())];
      let (objects, first) = commit_of(&files, &[], FIRST_DATE);
      let first_tree = tree_of(&objects);
      let version = push_through(state, (id, root), &objects, (None, first, first_tree));
      tagged(state, (id, root, version), first, "v1");
      let mut wants = fetch(&[first], &[]);
      // `include-tag` beside the request's other arguments, before its flush.
      let flush_at = wants.len() - FLUSH.len();
      wants.splice(flush_at..flush_at, packet(b"include-tag\n"));
      let began = super::hold_in(state, id, &owner()).expect("held");
      let start = answered(state, (id, &owner()), Asked::Request(&wants)).0;
      let in_turns = |state: &mut ShardState,
                      hold: super::GitGeneration,
                      held: bool,
                      land: &mut dyn FnMut(&mut ShardState)| {
        let mut admission = Admission::new(ROOM);
        let mut answer = super::lend_held_in(state, &hold, |served, packs| {
          upload::serve_in_turns(
            served,
            Version::V2,
            Asked::Request(&wants),
            packs,
            &mut admission,
          )
        })
        .expect("the request's step lent");
        let mut out = Vec::new();
        let mut judge = Judge::new("an answer in turns", answer_seen(&answer, &out));
        loop {
          let write = |served: &upload::Served<'_>, packs: &mut super::ShardPacks| {
            answer.write_within(
              served,
              packs,
              &mut admission,
              &mut out,
              MAX_LEN,
              &mut || true,
            )
          };
          let written = if held {
            super::lend_held_in(state, &hold, write)
          } else {
            lend_on(state, (id, None), write)
          }
          .expect("the answer's step lent");
          if judge.steps == 0 {
            land(state);
          }
          if written == Written::End {
            return out;
          }
          if let Err(stalled) = judge.stepped(answer_seen(&answer, &out)) {
            panic!("{stalled}: (bytes written, its pack's counters)")
          }
        }
      };
      let mut landed = false;
      let held = in_turns(state, began, true, &mut |state| {
        let files = [(b"README".to_vec(), b"two\n".to_vec())];
        let (objects, second) = commit_of(&files, &[first], FIRST_DATE + 1);
        let version = push_through(
          state,
          (id, root),
          &objects,
          (Some(first_tree), second, tree_of(&objects)),
        );
        tagged(state, (id, root, version), first, "v2");
        landed = true;
      });
      assert!(landed, "the push landed mid-answer");
      let hold = super::hold_in(state, id, &owner()).expect("held");
      let unheld = in_turns(state, hold, false, &mut |_| {});
      (start, held, unheld)
    });
  // Byte for byte: the encoder's level and window are one function of the bytes and the machine's measured rates
  // (`learn::Rates`), never the shard's clock, so two streams of one generation write one answer.
  assert_eq!(held, start, "held to the generation it began on");
  let said = |answer: &[u8]| sent_in_order(&pack_of(answer));
  assert_ne!(
    said(&unheld),
    said(&start),
    "lent the objects alone, an answer's later steps lose its refs"
  );
}

/// A-859 (main 01a20182's order of 2026-11-22 04:53: the whole-repository round must say which path a clone's objects
/// took). Do: push one commit to a git repository's `main` and fetch it, its stream's own counters read at its end.
/// Expect: the shard counts each of the stream's paths ([`super::STREAM_COUNTED`]) as the stream counted it, so the
/// daemon's status says which path a clone's objects took, and the fetch took one (its objects sent fresh, so the
/// counts are not all zero).
#[test]
fn t_a859_a_fetchs_stream_counts_each_path_its_objects_took_on_its_shard() {
  let (stream, counted) = crate::test_store::audit_on_stored_shard("vcs_store-upload", |state| {
    let (id, _, head) = pushed_repository(state, "counted", &[]);
    let (_, stream) = fetched_with_counters(state, (id, &owner()), &fetch(&[head], &[]));
    let counted: Vec<u64> = super::STREAM_COUNTED
      .iter()
      .map(|(name, _)| state.refusals.get(*name).copied().unwrap_or(0))
      .collect();
    (stream, counted)
  });
  let expected: Vec<u64> = super::STREAM_COUNTED
    .iter()
    .map(|(_, of)| of(&stream))
    .collect();
  assert_eq!(counted, expected, "{stream:?}");
  assert!(counted.iter().any(|count| *count > 0), "{stream:?}");
}

/// A-868 (main 01a20182's queue item 4: deflate states O(shards), counted). Do: on an audit shard, measure the git
/// rates on its thread (`Rates::measure`: the corpus deflated once at each of the ten levels), then fetch a pushed
/// commit (its step counts on the shard), then give back what the repository holds idle. Expect: the shard counts the
/// ten deflates' states, made or taken again; at most one made, the rest taken again; and what its thread kept let go
/// idle, counted dropped. Red before: ten made, one a level.
#[test]
fn t_a868_a_shards_deflate_states_are_counted_one_made_and_let_go_idle() {
  use super::{DEFLATE_STATES_DROPPED, DEFLATE_STATES_MADE, DEFLATE_STATES_REUSED};
  struct Elapsed(std::time::Instant);
  impl slates_git::store::learn::Clock for Elapsed {
    fn now_ns(&mut self) -> u64 {
      u64::try_from(self.0.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
  }
  crate::test_store::audit_on_stored_shard("vcs_store-deflaters", |state| {
    let (id, _, head) = pushed_repository(state, "deflated", &[]);
    slates_git::store::deflaters::let_go();
    super::count_deflaters(state);
    let names = [
      DEFLATE_STATES_MADE,
      DEFLATE_STATES_REUSED,
      DEFLATE_STATES_DROPPED,
    ];
    let counted =
      |state: &ShardState| names.map(|name| state.refusals.get(name).copied().unwrap_or(0));
    let before = counted(state);
    slates_git::store::learn::Rates::measure(&mut Elapsed(std::time::Instant::now()));
    fetched_with_counters(state, (id, &owner()), &fetch(&[head], &[]));
    let fetched = counted(state);
    crate::vcs_store::git_encode::give_back_idle(state, id);
    let idle = counted(state);
    let [made, reused, _] = [0, 1, 2].map(|at| fetched[at] - before[at]);
    let dropped = idle[2] - fetched[2];
    eprintln!("A-868: states made {made}, reused {reused}; dropped idle {dropped}");
    assert!(
      made + reused >= 10,
      "each level's deflate took a state: made {made}, reused {reused}"
    );
    assert!(made <= 1, "one state made: made {made}, reused {reused}");
    assert!(
      dropped >= 1,
      "the kept state let go idle: dropped {dropped}"
    );
  });
}

/// A-859 part A (main 01a20182's rulings of 2026-11-22 04:30 and 06:32: a fetched byte priced at its link, never at
/// memcpy; cold, the price where deflate at git's default level just pays; learned from the writes that waited for the
/// peer, from an answer's first wait on). Do: on an audit shard, read the fetch's exchange; teach the shard's pacing a
/// MiB less a byte of a slow link's writes (a microsecond a byte), past one write that did not wait; read the exchange
/// again; teach it a byte more; read it a third time. Expect: first the cold price (`Exchange::cold` of the machine's
/// rates), a byte dearer than memcpy prices one; still cold short of the learned bound, the write before the first wait
/// teaching nothing; then the link's own price, a microsecond a byte, and the learned bytes counted.
#[test]
fn t_a859_a_shard_prices_a_fetched_byte_cold_until_its_link_is_learned() {
  use super::{LINK_LEARNED, Pacing, Sent, exchange, learn_link};
  use slates_git::store::learn::Exchange;
  crate::test_store::audit_on_stored_shard("vcs_store-link", |state| {
    let rates = state.config.git_rates;
    let cold = exchange(state);
    let memcpy_priced = u128::from(cold.ns) * u128::from(state.config.memcpy_bytes_per_second)
      > u128::from(cold.bytes) * 1_000_000_000;
    let mut pacing = Pacing::default();
    let early = pacing.sent(Sent {
      bytes: 1 << 20,
      ns: 1,
      waited: false,
    });
    let slow = |bytes: u64| Sent {
      bytes,
      ns: bytes * 1_000,
      waited: true,
    };
    for taught in [pacing.sent(slow((1 << 20) - 1))].into_iter().flatten() {
      learn_link(state, taught);
    }
    let short = exchange(state);
    for taught in [pacing.sent(slow(1))].into_iter().flatten() {
      learn_link(state, taught);
    }
    let learned = exchange(state);
    let counted = state.refusals.get(LINK_LEARNED).copied();
    assert_eq!(
      (cold, memcpy_priced, early, short),
      (Exchange::cold(rates), true, None, Exchange::cold(rates)),
    );
    assert_eq!(
      (learned.ns, learned.bytes, counted),
      ((1 << 20) * 1_000, 1 << 20, Some(1 << 20)),
    );
  });
}

/// A-859 part A (main 01a20182's ruling of 2026-11-22 06:32: the loop's absence). Do: pace an answer made slowly, as a
/// starved shard makes one: many writes far apart, each into a socket with room (none waited), the time between them
/// long; then a link's price learned past its bound and halved past its memory. Expect: the slow answer teaches the
/// price nothing, so a clone slow for its own making never prices its link dearer; the halved price is the same
/// nanoseconds a byte.
#[test]
fn t_a859_an_answer_slow_for_its_own_making_teaches_its_link_price_nothing() {
  use super::{LinkPrice, Pacing, Sent};
  let mut pacing = Pacing::default();
  let taught: Vec<(u64, u64)> = (0..1_000)
    .filter_map(|_| {
      pacing.sent(Sent {
        bytes: 64 << 10,
        ns: 2_000,
        waited: false,
      })
    })
    .collect();
  assert!(taught.is_empty(), "{taught:?}");
  let mut price = LinkPrice::default();
  for _ in 0..200 {
    price.learn((1_000_000, 1 << 20));
  }
  let rates = slates_git::store::learn::Rates {
    deflate_ns_per_kib: [1; 10],
    deflate_saved_per_mille: [500; 10],
    search_ns_per_kib: 1,
  };
  let learned = price.exchange(rates).expect("learned");
  assert_eq!(learned.ns / learned.bytes, 0, "under a nanosecond a byte");
  assert_eq!(learned.ns * (1 << 20) / learned.bytes, 1_000_000);
}

/// Makes git repository `name` on `daemon`'s first shard, puts a pack of three windows and a part in the shard's content
/// store and lands its bytes on the node's store as a changeset's `Payload`, the creation and the landing each waited
/// for on this thread, so the shard is never held through the store's commit: the window, the pack's bytes, its length
/// and its root.
fn landed_pack(daemon: &crate::Daemon, name: &str) -> (usize, Vec<u8>, u64, slates_vcs::ContentId) {
  crate::git_push::tests::git_repository_on(daemon, name);
  let (disk, header, parts, landed) = crate::daemon::observe_first(daemon, |state| {
    let window = state
      .vcs_stores
      .content()
      .map_or(1 << 18, slates_vcs::ContentStore::window);
    let bytes: Vec<u8> = (0..3 * window + 100)
      .map(|at| u8::try_from(at % 251).unwrap())
      .collect();
    let len = u64::try_from(bytes.len()).unwrap();
    let root = crate::vcs_store::within_budget(state, len, |stores| {
      stores.content_mut().unwrap().put_object(&bytes).unwrap()
    });
    let header = slates_store::log::Header {
      tool: slates_store::log::Tool::Git,
      repository: 1,
      sequence: 0,
      judged_over: 0,
      grant: 0,
      work: 0,
      time_ns: slates_rt::futures::now_ns(),
      sections: 0,
    };
    let parts = vec![(slates_store::log::SectionKind::Payload, bytes.clone())];
    (
      state.vcs_disk.clone().unwrap(),
      header,
      parts,
      (window, bytes, len, root),
    )
  });
  assert!(crate::test_store::land_blocking(disk, header, parts));
  landed
}

/// Goal 93 item 2a (main 01a20182's ruling of 2026-11-22 05:45; w-git's driver hook: a fetch's and an encoding's packs
/// made resident before they read). Do: on a store-backed daemon, put a pack of three windows and a part in the shard's
/// content store, land its bytes on the node's store as a changeset's `Payload`, mark every window landed and let it
/// go; list what [`super::away_in`] finds of it, bring that back ([`super::brought_back_all`]), list again, and read the
/// pack whole. Expect: its four windows found let go, all brought back (their bytes the pack's), none found after, and
/// the pack read whole as it was put. Failing first: nothing made a repository's packs resident, so a clone's read of a
/// let-go window refused `Unread`.
#[test]
fn t_goal93_a_repositorys_let_go_pack_is_made_resident_before_it_is_read() {
  use slates_vcs::ContentId;
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (sent, got) = std::sync::mpsc::channel();
  let (window, bytes, len, root) = landed_pack(&daemon, "resident");
  crate::daemon::observe_first(&daemon, move |state| {
    crate::vcs_store::within_budget(state, 0, |stores| {
      let content = stores.content_mut().unwrap();
      let ids: Vec<ContentId> = bytes.chunks(window).map(ContentId::of).collect();
      for id in &ids {
        assert!(content.mark_landed(id) && content.let_go(id).is_ok());
      }
    });
    let away = super::away_in(
      state.vcs_stores.content().unwrap(),
      [(root, len)].into_iter(),
    );
    let task = slates_rt::futures::spawn(async move {
      let found = away.len();
      let brought = super::brought_back_all(away).await;
      let after = crate::state::with_state(|state| {
        super::away_in(
          state.vcs_stores.content().unwrap(),
          [(root, len)].into_iter(),
        )
        .len()
      });
      let read = crate::state::with_state(|state| {
        crate::vcs_store::lent_to_read((&mut state.store, &mut state.vcs_stores), |stores| {
          stores
            .content()
            .and_then(|content| content.object_bytes(&root, len))
        })
      })
      .flatten();
      let _ = sent.send((found, brought, after, read == Some(bytes)));
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  let (found, brought, after, whole) = got.recv().expect("made resident");
  daemon.stop();
  assert_eq!((found, after, whole), (4, Some(0), true));
  assert!(brought > 0, "{brought}");
}

/// A-864 piece 2 (main 01a20182's ruling of 2026-11-22: residency bounded by a turn's windows, never a pack's). Do: on a
/// store-backed daemon, put a pack of four windows in the shard's content store, land its bytes on the node's store and
/// let every window go; run a fetch's look-ahead for a next turn beginning at the pack's first byte; then for one that
/// stopped at its last window. Expect: the look-ahead brings back the first two windows alone (`LOOK_AHEAD_WINDOWS`), the
/// last two still let go; then the window the turn stopped at, the third still let go; each counted.
#[test]
fn t_a864_a_fetchs_look_ahead_brings_back_a_turns_windows_never_its_pack() {
  use slates_git::store::packed::Waited;
  use slates_vcs::ContentId;
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (sent, got) = std::sync::mpsc::channel();
  let (window, bytes, len, root) = landed_pack(&daemon, "look-ahead");
  crate::daemon::observe_first(&daemon, move |state| {
    crate::vcs_store::within_budget(state, 0, |stores| {
      let content = stores.content_mut().unwrap();
      for id in bytes.chunks(window).map(ContentId::of) {
        assert!(content.mark_landed(&id) && content.let_go(&id).is_ok());
      }
    });
    let step = u64::try_from(window).unwrap();
    let away = move |state: &mut crate::state::ShardState| -> Vec<bool> {
      let content = state.vcs_stores.content().unwrap();
      (0..4u64)
        .map(|index| content.away_window(&root, len, index * step).is_some())
        .collect()
    };
    let task = slates_rt::futures::spawn(async move {
      let at = |offset| Waited {
        pack: root,
        len,
        offset,
      };
      let mut resident = Vec::new();
      super::read_ahead(
        crate::vcs_http::git::upload::PackReads {
          next: Some(at(0)),
          stopped: None,
        },
        &mut resident,
      )
      .await;
      let ahead = crate::state::with_state(away);
      super::read_ahead(
        crate::vcs_http::git::upload::PackReads {
          next: None,
          stopped: Some(at(3 * step + 7)),
        },
        &mut resident,
      )
      .await;
      let stopped = crate::state::with_state(away);
      let counted = crate::state::with_state(|state| {
        (
          state.refusals.get(super::AHEAD_BROUGHT).copied(),
          state.refusals.get(super::STOPPED_BROUGHT).copied(),
        )
      });
      let _ = sent.send((ahead, stopped, counted));
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  let (ahead, stopped, counted) = got.recv().expect("looked ahead");
  daemon.stop();
  assert_eq!(ahead, Some(vec![false, false, true, true]));
  assert_eq!(stopped, Some(vec![false, false, true, false]));
  assert_eq!(counted, Some((Some(2), Some(1))));
}

/// A-864 piece 3 (main 01a20182's ruling of 2026-11-22: residency at its peak a turn's windows, never a pack's). Do: on a
/// store-backed daemon, let a four-window pack go as the look-ahead test does; bring back what a fetch's next turn reads
/// from its first byte; then let go behind a cursor in its third window. Expect: the first two windows held while the
/// cursor reads them, the answer's resident set two; once the cursor has passed them, both handed to
/// `vcs_store::let_go_read` and let go again, counted (no home pins a window its shard does not hold for the region:
/// main 01a20182's ruling of 2026-11-22 21:15), the resident set empty; the most held at once
/// (`git_resident.peak_windows`) two.
#[test]
fn t_a864_a_fetch_lets_go_again_the_windows_its_cursor_has_passed() {
  use slates_git::store::packed::Waited;
  use slates_vcs::ContentId;
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (sent, got) = std::sync::mpsc::channel();
  let (window, bytes, len, root) = landed_pack(&daemon, "behind");
  crate::daemon::observe_first(&daemon, move |state| {
    crate::vcs_store::within_budget(state, 0, |stores| {
      let content = stores.content_mut().unwrap();
      for id in bytes.chunks(window).map(ContentId::of) {
        assert!(content.mark_landed(&id) && content.let_go(&id).is_ok());
      }
    });
    let step = u64::try_from(window).unwrap();
    let away = move |state: &mut crate::state::ShardState| -> Vec<bool> {
      let content = state.vcs_stores.content().unwrap();
      (0..4u64)
        .map(|index| content.away_window(&root, len, index * step).is_some())
        .collect()
    };
    let task = slates_rt::futures::spawn(async move {
      let at = |offset| Waited {
        pack: root,
        len,
        offset,
      };
      let mut resident = Vec::new();
      super::read_ahead(
        crate::vcs_http::git::upload::PackReads {
          next: Some(at(0)),
          stopped: None,
        },
        &mut resident,
      )
      .await;
      let held = (crate::state::with_state(away), resident.len());
      let after = crate::state::with_state(|state| {
        super::let_go_behind(state, &mut resident, Some(at(2 * step + 1)));
        (
          away(state),
          state.refusals.get(super::LET_GO_BEHIND).copied(),
          state.refusals.get(super::PEAK_WINDOWS).copied(),
        )
      });
      let _ = sent.send((held, after, resident.len()));
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  let (held, after, left) = got.recv().expect("let go behind");
  daemon.stop();
  assert_eq!(held, (Some(vec![false, false, true, true]), 2));
  assert_eq!(
    after,
    Some((vec![true, true, true, true], Some(2), Some(2)))
  );
  assert_eq!(left, 0);
}

/// Shape: the commits of [`a911_the_walk_resolves_each_id_met_again_once_not_in_every_trees_version`]'s line: enough
/// that its trees' entries over all versions far outnumber its objects.
const LINE_COMMITS: usize = 40;

/// Shape: the files of that line's root tree, one changed a commit: a tree whose versions repeat most of its entries.
const LINE_FILES: usize = 60;

/// A git repository `name` whose `main` is a line of [`LINE_COMMITS`] commits, each a root tree of [`LINE_FILES`] files
/// with one file changed from the commit before: the repository, its head, and its distinct objects.
fn pushed_line(state: &mut ShardState, name: &str) -> (RepositoryId, ObjectId, usize) {
  let (id, root) = repository(state, name, VcsTool::Git);
  let mut files: Vec<(Vec<u8>, Vec<u8>)> = (0..LINE_FILES)
    .map(|at| {
      (
        format!("file-{at:03}").into_bytes(),
        format!("{at} at 0\n").into_bytes(),
      )
    })
    .collect();
  let mut distinct = std::collections::BTreeSet::new();
  let (mut parent, mut base): (Option<ObjectId>, Option<ObjectId>) = (None, None);
  for commit in 0..LINE_COMMITS {
    let changed = commit % LINE_FILES;
    files[changed].1 = format!("{changed} at {commit}\n").into_bytes();
    let parents: Vec<ObjectId> = parent.into_iter().collect();
    let date = FIRST_DATE + u64::try_from(commit).unwrap();
    let (objects, head) = commit_of(&files, &parents, date);
    distinct.extend(objects.iter().map(|(_, id, _)| *id));
    let tree = tree_of(&objects);
    push_through(state, (id, root), &objects, (base, head, tree));
    (parent, base) = (Some(head), Some(tree));
  }
  (id, parent.expect("a head"), distinct.len())
}

/// Shape: a `tree:<depth>` filter's depth past every object of [`pushed_line`]'s line (its blobs are at depth 1), so the
/// filter sends everything yet is no per-object filter: no run of versions answers its want, and the walk chooses each
/// object (`slates_git::negotiation::Filter::is_per_object`; the bench's clones meet the same walk through wants no run
/// answers).
const PAST_THE_LINE: u64 = 5;

/// A `fetch` request for `want` under `filter tree:<depth>`, with no progress.
fn fetch_under_depth(want: ObjectId, depth: u64) -> Vec<u8> {
  let arguments = [
    format!("want {want}"),
    format!("filter tree:{depth}"),
    "no-progress".to_owned(),
    "done".to_owned(),
  ];
  request("fetch", &arguments)
}

/// A fetch's counts of its walk's ids: resolved through the index, answered by its cache, and of the first those whose
/// slot held another id.
type PositionCounted = (u64, u64, u64);

/// The shard's [`PositionCounted`] now.
fn position_counts(state: &ShardState) -> PositionCounted {
  let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
  (
    count(super::POSITION_LOOKUPS),
    count(super::POSITION_CACHE_HITS),
    count(super::POSITION_CACHE_CONFLICTS),
  )
}

/// [`pushed_line`]'s line, as repository `name` on an audit shard, fetched whole under a depth filter past it: its
/// distinct objects, what the fetch counted ([`PositionCounted`]), and the objects its pack sent.
fn line_fetched(name: &'static str) -> (usize, PositionCounted, usize) {
  crate::test_store::audit_on_stored_shard(name, move |state| {
    let (id, head, distinct) = pushed_line(state, name);
    let before = position_counts(state);
    let asked = fetch_under_depth(head, PAST_THE_LINE);
    let (answer, _) = fetched_with_counters(state, (id, &owner()), &asked);
    let after = position_counts(state);
    let sent = ids_in(&pack_of(&answer)).len();
    let counted = (after.0 - before.0, after.1 - before.1, after.2 - before.2);
    (distinct, counted, sent)
  })
}

/// A-911 (main 01a20182, 2026-11-24: a partial clone's walk before its pack spent 22,771 of 23,095 samples resolving
/// tree entries to positions through the index, again in every version of a tree). Do: push a line of [`LINE_COMMITS`]
/// commits whose root tree of [`LINE_FILES`] files changes one file a commit, then fetch its head whole under a depth
/// filter past the line (so the walk, not a run of versions, chooses its objects), the shard's counts read around it. Expect: the walk met every entry of every version (lookups and the cache's hits together at
/// least commits × files, so it is the walk this measures); but resolved through the index only about once an object
/// met (at most three times the line's distinct objects), not once an entry of every version; and the pack whole.
#[test]
fn a911_the_walk_resolves_each_id_met_again_once_not_in_every_trees_version() {
  let (distinct, (lookups, hits, _), sent) = line_fetched("a911-line");
  let met = u64::try_from(LINE_COMMITS * LINE_FILES).unwrap();
  let distinct_objects = u64::try_from(distinct).unwrap();
  eprintln!(
    "a911: {lookups} lookups, {hits} hits, {distinct} distinct objects, {sent} sent, {met} entries met"
  );
  assert_eq!(sent, distinct, "the pack whole");
  assert!(
    lookups + hits >= met,
    "the walk met every entry: {lookups} + {hits} of {met}"
  );
  assert!(
    lookups <= 3 * distinct_objects,
    "resolved about once an object, not an entry a version: {lookups} lookups for {distinct} objects"
  );
}

/// A-911's miss counter (main 01a20182's order of 2026-11-25: whether a fetch's misses are ids its direct-mapped cache
/// put out of a slot another took, or ids met past what it holds, decides between more ways a slot and a wider window).
/// Do: fetch [`pushed_line`]'s line whole under a depth filter past it, as the A-911 test does. Expect: of the ids
/// resolved through the index, some counted conflicts (60 ids a commit in 512 slots: about one in nine shares its slot,
/// and those two put each other out every commit), never more than the lookups. Red before: none counted.
#[test]
fn a911_a_miss_into_a_slot_another_id_holds_is_counted_a_conflict() {
  let (_, (lookups, _, conflicts), _) = line_fetched("a911-conflicts");
  eprintln!("a911: {lookups} lookups, {conflicts} of them conflicts");
  assert!(conflicts > 0, "conflicts counted: {conflicts} of {lookups}");
  assert!(
    conflicts <= lookups,
    "a conflict is a lookup: {conflicts} of {lookups}"
  );
}

/// [`pushed_line`]'s line, as repository `name` on a fresh audit shard, fetched whole under a depth filter past it: the
/// most entries one unit of its walk took, and the longest tree one unit parsed, as the shard counted them most.
fn line_walk_most(name: &'static str) -> (u64, u64) {
  crate::test_store::audit_on_stored_shard(name, move |state| {
    let (id, head, _) = pushed_line(state, name);
    let asked = fetch_under_depth(head, PAST_THE_LINE);
    let _ = fetched_with_counters(state, (id, &owner()), &asked);
    let most = |counter: &str| state.refusals.get(counter).copied().unwrap_or(0);
    (
      most(super::UNIT_ENTRIES_MOST),
      most(super::TREE_PARSE_BYTES_MOST),
    )
  })
}

/// The bounded-work rule in a fetch's walk (main 01a20182's order of 2026-11-25: 280,392 polls past the step quantum in
/// one clone of git/git, the walk opening a tree whole in one unit). Do: fetch [`pushed_line`]'s line, its root tree
/// [`LINE_FILES`] entries, whole under a depth filter past it. Expect: no unit of the walk took more than one entry
/// before asking whether its step was spent (a tree's entries taken one a unit, as the `sparse:oid` walk takes them),
/// and the trees it parsed counted (so it is the walk this measures). Red before: one unit took the root's 60.
#[test]
fn a_fetchs_walk_takes_one_entry_a_unit_never_a_tree_whole() {
  let (entries_most, tree_bytes_most) = line_walk_most("one-entry-a-unit");
  eprintln!(
    "walk: {entries_most} entries a unit at most, the longest tree {tree_bytes_most} bytes"
  );
  assert!(tree_bytes_most > 0, "trees parsed: {tree_bytes_most} bytes");
  assert_eq!(entries_most, 1, "one entry a unit, never a tree whole");
}
