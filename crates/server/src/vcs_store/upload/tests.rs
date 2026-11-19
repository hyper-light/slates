//! A-682's tests: the daemon's lender over A-662's map, driven as upload-pack's edges drive it, each on a real
//! one-shard daemon whose repository took its objects through A-635's append: a pushed branch listed and fetched whole,
//! a ref the reader may not read neither listed nor wanted, a name that is no git ref not lent, a store not held here
//! not lent, and an SSH session the SSH site's services start over the daemon's lender.

use std::sync::mpsc;
use std::time::Duration;

use slates_db::catalog::{Principal, RepositoryId, RepositoryRecord, VolumeId as DbVolumeId};
use slates_git::Admission;
use slates_git::http::Version;
use slates_git::object_name::{ObjectFormat, ObjectId};
use slates_git::pktline::{self, MAX_LEN, Packet};
use slates_git::unpack::{self, NoIndex};
use slates_git::upload::{self, Asked, Refusal, Written};
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
use crate::vcs_ssh::site::{Command, Next, Service, Services, Start, ToolServices};

/// Shape: the room a request and its answer are admitted: far past the tests' few small objects.
const ROOM: u64 = 1 << 20;
/// Shape: the most steps an answer or a session's request may take here, far past a few small objects' packets: a
/// bound, so an answer that never ended would fail the test rather than hold it.
const STEPS: usize = 1 << 12;
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
fn request(command: &str, arguments: &[String]) -> Vec<u8> {
  let mut bytes = packet(format!("command={command}\n").as_bytes());
  bytes.extend_from_slice(b"0001");
  for argument in arguments {
    bytes.extend(packet(format!("{argument}\n").as_bytes()));
  }
  bytes.extend_from_slice(FLUSH);
  bytes
}

/// An `ls-refs` request for `HEAD` and the branches, each symref's target named.
fn ls_refs() -> Vec<u8> {
  let arguments = ["symrefs", "ref-prefix HEAD", "ref-prefix refs/heads/"];
  request("ls-refs", &arguments.map(str::to_owned))
}

/// A `fetch` request: `wants` by id, `refs` by name, and `done`, with no progress.
fn fetch(wants: &[ObjectId], refs: &[&str]) -> Vec<u8> {
  let mut arguments: Vec<String> = wants.iter().map(|id| format!("want {id}")).collect();
  arguments.extend(refs.iter().map(|name| format!("want-ref {name}")));
  arguments.extend(["no-progress".to_owned(), "done".to_owned()]);
  request("fetch", &arguments)
}

/// What `reader` is answered for `asked` of repository `id` on the shard, as an edge serves it: the request's step lent
/// the refs `reader` may read, then each step of the answer lent the objects alone, one pkt-line's room a step. The
/// answer's bytes and why it was refused, if it was.
fn answered(
  state: &mut ShardState,
  (id, reader): (RepositoryId, &Principal),
  asked: Asked<'_>,
) -> (Vec<u8>, Option<Refusal>) {
  let mut admission = Admission::new(ROOM);
  let mut answer = lend_on(state, (id, Some(reader)), |served, packs| {
    upload::serve(served, Version::V2, asked, packs, &mut admission)
  })
  .expect("the request's step lent");
  let mut out = Vec::new();
  for _ in 0..STEPS {
    let written = lend_on(state, (id, None), |served, packs| {
      answer.write(served, packs, &mut admission, &mut out, MAX_LEN)
    })
    .expect("the answer's step lent");
    if written == Written::End {
      return (out, answer.refusal().cloned());
    }
  }
  panic!("an answer that never ended: {out:?}")
}

/// The data packets of `bytes` up to the first flush, each whole; a stream that ends before one fails the test.
fn lines_of(bytes: &[u8]) -> Vec<Vec<u8>> {
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
fn pack_of(answer: &[u8]) -> Vec<u8> {
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

/// The ids of `objects`, in name order.
fn ids_of(objects: &[Brought]) -> Vec<String> {
  let mut ids: Vec<String> = objects.iter().map(|(_, id, _)| id.to_string()).collect();
  ids.sort();
  ids
}

/// `ls-refs`' line for `name` at `head`, its symref's target `target` when it has one.
fn ref_line(head: ObjectId, name: &str, target: Option<&str>) -> Vec<u8> {
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
  let (listed, fetched, objects, head) = crate::daemon::audit_on_shard(|state| {
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

/// A-682 (§4.17.1 "Policy": a ref lent only to a reader its rule lets read it, a ref no rule speaks to its owner's
/// alone). Do: on a repository whose one rule lets `READER` read `refs/heads/main`, push a commit to `main` and make
/// `topic` at it; ask `ls-refs` as the owner and as the reader; then, as the reader, fetch `refs/heads/topic` by name.
/// Expect: the owner listed `HEAD`, `main` and `topic`; the reader `HEAD` and `main` alone; the reader's `want-ref`
/// refused as git refuses a ref it does not have ("unknown ref").
#[test]
fn t_a682_a_ref_the_reader_may_not_read_is_neither_listed_nor_wanted() {
  let (by_owner, by_reader, wanted, head) = crate::daemon::audit_on_shard(|state| {
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
  let (listed, head) = crate::daemon::audit_on_shard(|state| {
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
  let lent = crate::daemon::audit_on_shard(|state| {
    let (mercurial, _) = repository(state, "elsewhere", VcsTool::Mercurial);
    let reply = crate::verbs::dispatch(
      state,
      1,
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
  for _ in 0..STEPS {
    let stepped = command
      .step()
      .await
      .map_err(|refused| format!("{refused:?}"))?;
    let out = command.output().1.to_vec();
    command.sent(0, out.len());
    written.extend(out);
    if stepped.next != Next::More {
      return Ok((written, stepped.next));
    }
  }
  Err(format!("a step bound passed: {written:?}"))
}

/// One SSH session of git's upload-pack over `record`'s repository, as the SSH site's services start it over the
/// daemon's lenders (git's, and Mercurial's beside it, A-679) for its owner at protocol v2: its advertisement, then each
/// of `requests`' answers, each with what the command waits for after it; none when the services would not start it.
async fn ssh_session(
  record: &RepositoryRecord,
  requests: &[Vec<u8>],
) -> Result<Vec<(Vec<u8>, Next)>, String> {
  let mut services = ToolServices::new(ShardStores, ServedViews, ROOM);
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
  let daemon = crate::daemon::audit_daemon();
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
