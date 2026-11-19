use std::future::Future;
use std::task::{Context, Poll, Waker};

use slates_db::HostId;
use slates_db::catalog::{
  ObjectFormat, Principal, RepositoryId, RepositoryPolicy, RepositoryRecord, VcsTool,
};
use slates_git::http::Version;
use slates_git::object_name::ObjectFormat as GitFormat;
use slates_git::pktline::{self, Packet};
use slates_git::store::Repository as GitStore;
use slates_git::upload::{PackPlan, PackRefused, PackSource, Served};
use slates_vcs::ContentStore;

use super::super::site::{
  Command, GIT_DIE_STATUS, Next, Refused, Service, Services, Start, Stepped, ToolServices, Unserved,
};
use super::UploadCommand;
use crate::vcs_http::git::upload::StoreLender;

/// Shape: the content store's budget and window the test's empty repository is kept in.
const STORE_BUDGET: u64 = 1 << 20;
/// Shape: as [`STORE_BUDGET`]: a window of many identities.
const STORE_WINDOW: usize = 1 << 16;
/// Shape: the branch the test repository's `HEAD` names, unborn.
const UNBORN_HEAD: &[u8] = b"refs/heads/main";
/// Shape: the room each request is admitted: more than any request here.
const ROOM: u64 = 1 << 20;
/// Shape: the repository whose store the test lender holds.
const HELD: RepositoryId = RepositoryId { bytes: [0; 16] };
/// Shape: a v2 `ls-refs` request, whole.
const LS_REFS: &[u8] = b"0014command=ls-refs\n0000";
/// Shape: a flush alone, with which a client ends its session.
const FLUSH: &[u8] = b"0000";
/// Shape: [`LS_REFS`]'s first packet alone, whole, its flush not yet sent.
const LS_REFS_FIRST_PACKET: &[u8] = b"0014command=ls-refs\n";

/// An empty SHA-1 repository's store, kept for the test's life.
struct EmptyRepository {
  /// Its index, versions and greens: none yet.
  repository: GitStore,
  /// Its content: none yet.
  store: ContentStore,
}

/// What [`NoPacks`] answers if asked: no session of these tests begins a pack, so the refusal is never read. Past
/// admission is the plainest one slates-git has (A-640 part 3 removed the stall these tests gave before), as the HTTP
/// edge's tests answer.
fn no_pack() -> PackRefused {
  PackRefused::PastAdmission(slates_git::PastAdmission { asked: 1, left: 0 })
}

/// A pack source no session of these tests begins: an empty repository has nothing to pack.
#[derive(Clone, Copy, Debug)]
struct NoPacks;

impl PackSource for NoPacks {
  type Pack = ();

  fn begin(
    &mut self,
    _served: &Served<'_>,
    _plan: &PackPlan<'_>,
    _admission: &mut slates_git::Admission,
  ) -> Result<(), PackRefused> {
    Err(no_pack())
  }

  fn write(
    &mut self,
    _served: &Served<'_>,
    _pack: &mut (),
    _out: &mut Vec<u8>,
    _room: usize,
  ) -> Result<bool, PackRefused> {
    Err(no_pack())
  }
}

/// Lends the repository [`HELD`]'s store, an empty one, and no other.
#[derive(Clone, Copy)]
struct Lends {
  /// The store, kept for the test's life.
  held: &'static EmptyRepository,
  /// The pack source.
  packs: NoPacks,
}

impl Lends {
  /// A lender of a fresh empty repository's store.
  fn empty() -> Lends {
    let held = Box::leak(Box::new(EmptyRepository {
      repository: GitStore::new(GitFormat::Sha1),
      store: ContentStore::new(STORE_BUDGET, STORE_WINDOW).expect("a content store"),
    }));
    Lends {
      held,
      packs: NoPacks,
    }
  }
}

impl StoreLender for Lends {
  type Packs = NoPacks;

  fn holds(&self, id: RepositoryId) -> bool {
    id == HELD
  }

  fn lend<R>(
    &mut self,
    id: RepositoryId,
    step: impl FnOnce(&Served<'_>, &mut NoPacks) -> R,
  ) -> Option<R> {
    let held = self.held;
    let served = Served {
      repository: &held.repository,
      store: &held.store,
      refs: &[],
      head: UNBORN_HEAD,
    };
    self.holds(id).then(|| step(&served, &mut self.packs))
  }
}

/// `future`'s output, polled once: the command's steps never wait.
fn ready<F: Future>(future: F) -> F::Output {
  let mut future = std::pin::pin!(future);
  match future
    .as_mut()
    .poll(&mut Context::from_waker(Waker::noop()))
  {
    Poll::Ready(output) => output,
    Poll::Pending => panic!("a step that waits"),
  }
}

/// Steps `command` until it waits or exits, as the edge does, taking all it writes: what it wrote, what it consumed, and
/// what it waits for.
fn run(command: &mut UploadCommand<Lends>) -> (Vec<u8>, usize, Next) {
  let mut written = Vec::new();
  let mut consumed = 0;
  loop {
    let Stepped {
      consumed: took,
      next,
    } = ready(command.step()).expect("a step");
    consumed += took;
    let out = command.output().1.to_vec();
    command.sent(0, out.len());
    written.extend(out);
    if next != Next::More {
      return (written, consumed, next);
    }
  }
}

/// Whether `haystack` holds `needle`.
fn holds(haystack: &[u8], needle: &[u8]) -> bool {
  haystack
    .windows(needle.len())
    .any(|window| window == needle)
}

/// A-651 part 2 (A-640 part 2's upload-pack over SSH; gitprotocol-v2). Do: start a v2 session over an empty repository's
/// store, then send an `ls-refs` request, then a flush alone. Expect: the advertisement first, unasked (`version 2`, with
/// `ls-refs=unborn`); the `ls-refs` answer the flush alone, the request's 24 bytes consumed; and the flush ending the
/// session, exit 0.
#[test]
fn an_ssh_session_advertises_answers_each_request_and_ends_at_a_flush() {
  let mut command = UploadCommand::new(Lends::empty(), HELD, Version::V2, ROOM);
  let (advertisement, _, waiting) = run(&mut command);
  command.input(LS_REFS).expect("taken");
  let ls_refs = run(&mut command);
  command.input(FLUSH).expect("taken");
  let ended = run(&mut command);
  assert!(
    advertisement.starts_with(b"000eversion 2\n"),
    "{advertisement:?}"
  );
  assert!(holds(&advertisement, b"ls-refs=unborn"));
  assert_eq!(
    (waiting, ls_refs, ended),
    (
      Next::Input,
      (FLUSH.to_vec(), LS_REFS.len(), Next::Input),
      (Vec::new(), FLUSH.len(), Next::Exited(0))
    )
  );
}

/// A-651 part 2 (git's server dies on a request it refuses, `serve.c`). Do: after the advertisement, send bytes that are
/// no pkt-line. Expect: git's `ERR` answer, every byte consumed, and the session ended with git's `die` status.
#[test]
fn a_refused_request_is_answered_in_gits_words_and_ends_the_session() {
  let mut command = UploadCommand::new(Lends::empty(), HELD, Version::V2, ROOM);
  let _ = run(&mut command);
  command.input(b"zzzz").expect("taken");
  let (answer, consumed, next) = run(&mut command);
  assert!(holds(&answer, b"ERR "), "{answer:?}");
  assert_eq!((consumed, next), (4, Next::Exited(GIT_DIE_STATUS)));
}

/// A-651 part 2 (slates-git answers protocols v0 and v1 statelessly alone, A-640 part 3, and a v0 client over SSH
/// negotiates in rounds on one stream, a session owed). Do: start a session at protocol v0 (no `GIT_PROTOCOL`), then one
/// at v1. Expect: each answered with one whole `ERR` packet naming protocol v2 and no advertisement, nothing consumed,
/// and the session ended with git's `die` status.
#[test]
fn a_v0_or_v1_session_is_refused_until_its_stateful_session_is_served() {
  let refused = |version| {
    let mut command = UploadCommand::new(Lends::empty(), HELD, version, ROOM);
    let (answer, consumed, next) = run(&mut command);
    let packet = match pktline::read(&answer, 0) {
      Ok(Some((Packet::Data(data), length))) if length == answer.len() => data.to_vec(),
      _ => Vec::new(),
    };
    let names_v2 = packet.starts_with(b"ERR ") && holds(&packet, b"protocol v2");
    (names_v2, consumed, next)
  };
  let expected = (true, 0, Next::Exited(GIT_DIE_STATUS));
  assert_eq!(
    (refused(Version::V0), refused(Version::V1)),
    (expected, expected)
  );
}

/// A-651 part 2 (`serve.c` `process_request` peeks a request's first packet under `PACKET_READ_GENTLE_ON_EOF`). Do:
/// after the advertisement, send half of a request's first packet and end the input. Expect: nothing answered and the
/// session ended, exit 0, as git's server ends when its input ends before a request's first packet is whole.
#[test]
fn the_end_of_input_before_a_whole_request_ends_the_session() {
  let mut command = UploadCommand::new(Lends::empty(), HELD, Version::V2, ROOM);
  let _ = run(&mut command);
  command.input(b"0014comm").expect("taken");
  let waiting = run(&mut command);
  command.input_end();
  let ended = run(&mut command);
  assert_eq!(
    (waiting, ended),
    (
      (Vec::new(), 0, Next::Input),
      (Vec::new(), 0, Next::Exited(0))
    )
  );
}

/// A-651 part 2 (git's reader is gentle on the end of its input only before a request's first packet is whole:
/// `serve.c` `process_request` clears `PACKET_READ_GENTLE_ON_EOF` once it has peeked that packet, and `pkt-line.c`
/// `get_packet_data` dies at a later short read, at `e1ec09b2`). Do: after the advertisement, send a request's first
/// packet whole, `command=ls-refs`, and end the input before its flush. Expect: nothing answered or consumed, and the
/// session refused in git's words, "the remote end hung up unexpectedly", with git's `die` status.
#[test]
fn the_end_of_input_inside_a_request_dies_as_gits_reader_does() {
  let mut command = UploadCommand::new(Lends::empty(), HELD, Version::V2, ROOM);
  let _ = run(&mut command);
  command.input(LS_REFS_FIRST_PACKET).expect("taken");
  let waiting = run(&mut command);
  command.input_end();
  let ended = ready(command.step());
  assert_eq!(
    (waiting, ended),
    (
      (Vec::new(), 0, Next::Input),
      Err(Refused {
        stderr: b"fatal: the remote end hung up unexpectedly\n".to_vec(),
        exit_status: GIT_DIE_STATUS,
      })
    )
  );
}

/// The git repository `id`, the owner's, with no rules.
fn record(id: RepositoryId) -> RepositoryRecord {
  RepositoryRecord {
    id,
    name: "r.git".to_owned(),
    tool: VcsTool::Git,
    format: ObjectFormat::Sha1,
    default_branch: None,
    owner: Principal::Uid {
      host: HostId(0),
      uid: 1,
    },
    policy: RepositoryPolicy {
      version: 1,
      rules: Vec::new(),
      hooks: Vec::new(),
      hooks_env: Vec::new(),
    },
  }
}

/// A-651 part 2 (the site's step 3). Do: start upload-pack for a repository whose store the lender holds, for one whose
/// store it does not hold, and receive-pack and Mercurial's session for the held one. Expect: upload-pack started for the
/// held store alone; every other start not served.
#[test]
fn upload_pack_starts_only_over_a_store_the_lender_holds() {
  let mut services = ToolServices::new(Lends::empty(), ROOM);
  let held = record(HELD);
  let unheld = record(RepositoryId { bytes: [1; 16] });
  let principal = held.owner.clone();
  let answers = [
    (Service::GitUploadPack, &held),
    (Service::GitUploadPack, &unheld),
    (Service::GitReceivePack, &held),
    (Service::Mercurial { hidden: false }, &held),
  ]
  .map(|(service, repository)| {
    let start = Start {
      service,
      repository: Some(repository),
      principal: &principal,
      git_protocol: Some(&b"version=2"[..]),
    };
    services.start(&start).err()
  });
  assert_eq!(
    answers,
    [None, Some(Unserved), Some(Unserved), Some(Unserved)]
  );
}
