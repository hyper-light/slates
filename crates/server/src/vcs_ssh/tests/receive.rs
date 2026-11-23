//! A-683's tests (A-654's SSH call; §4.17.2 "A push" and "Transports" item 2; the SSH receive-pack module's docs):
//! stock git 2.50.1's push, captured by `crates/git/fixtures/push/make-push-fixtures.sh`, sent over an SSH session to
//! the edge's production site and services on an audit daemon's shard, the repository's owner shard, where A-654 part
//! 2's driver lands it; the session's protocols, v1's `version 1` line and v2 answered at v0, with a flush alone, git's
//! push of nothing, answered with nothing; and the command's refusals before its push starts, a shard not lent and a
//! repository gone, each in its words.

use std::future::Future;
use std::task::{Context, Poll, Waker};

use slates_db::catalog::{Principal, RepositoryId};
use slates_git::http::Version;
use slates_git::pktline::{self, Packet};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4, TcpListener};
use slates_ssh::userauth::{Holding, KeyHolders, UserKey};

use super::{
  CLIENT_CHANNEL, CLIENT_PACKET, CLIENT_WINDOW, HELD_SEED, HOST_SEED, ONE_SEAT, Peer, ROOM_BYTES,
  TICK_NS, Transcript, blob, put_string, query, send_data, sign, transcript,
};
use crate::config::GIT_KEEPALIVE_NS;
use crate::daemon::observe_first;
use crate::git_push::tests::{chain, files, git_repository, gits_report, owner, text};
use crate::git_push::{ReceivePack, Unpresented};
use crate::state::ShardState;
use crate::vcs_http::Bounds;
use crate::vcs_http::ShardRepositories;
use crate::vcs_http::git::{ShardLender, ThisShard};
use crate::vcs_ssh::receive::ReceiveCommand;
use crate::vcs_ssh::site::{ClientGone, Command, GIT_DIE_STATUS, Refused, ToolServices, ToolSite};
use crate::vcs_ssh::{EdgeState, serve};
use crate::vcs_store::hg_view::ServedViews;
use crate::vcs_store::upload::ShardStores;

/// Shape: the room each push is admitted: far past the fixture's bytes and what reading it builds (the driver's tests'
/// room).
const ROOM: u64 = 1 << 24;

/// Shape: the bytes the client sends at a time: fewer than any request holds and no multiple of a packet's length, so
/// the push arrives over many channel messages and some packet across two (the driver's tests' piece).
const PIECE: usize = 97;

/// Shape: the repository's name.
const NAME: &str = "pushed";

/// Shape: the command a client runs for it, its path as git's client sends one for `ssh://host/pushed`.
const COMMAND: &[u8] = b"git-receive-pack '/pushed'";

/// Shape: a repository no partition holds: one destroyed after the site found it.
const GONE: RepositoryId = RepositoryId { bytes: [0xEE; 16] };

/// The held key proves the repositories' owner, the principal the driver's own tests push as; no other key proves
/// anyone.
#[derive(Clone)]
struct OwnerKeys;

impl KeyHolders for OwnerKeys {
  type Principal = Principal;

  fn holding(&mut self, _user: &[u8], _key: &UserKey, key_blob: &[u8]) -> Holding<Principal> {
    if key_blob == blob(&HELD_SEED) {
      Holding::Holds {
        principal: owner(),
        keys_required: 1,
      }
    } else {
      Holding::Nobody
    }
  }
}

/// The edge's production site: the owner's key, repositories found on the shard's own partition as the HTTP edge finds
/// them, and the services the daemon's sites run: upload-pack lent each store from A-662's map ([`ShardStores`], A-682),
/// `serve --stdio` each served view ([`ServedViews`], A-680 part 2a), and each push lent the state of the shard the
/// connection runs on ([`ThisShard`]) and admitted [`ROOM`]; git's keepalive written after `keepalive_ns` of a silent
/// fetch answer.
fn site_within(
  room: u64,
  keepalive_ns: u64,
) -> ToolSite<OwnerKeys, ShardRepositories, DaemonServices> {
  ToolSite::new(
    OwnerKeys,
    ShardRepositories,
    ToolServices::new(ShardStores, ServedViews, ThisShard, room).with_git_keepalive(keepalive_ns),
  )
}

/// The daemon's services ([`site_within`]).
type DaemonServices = ToolServices<ShardStores, ServedViews, ThisShard>;

/// Makes git repository [`NAME`] on the shard and serves the edge there with [`site_within`], its listener on a loopback port:
/// the repository, the listener's address, and the advertisement the driver makes the owner now (git's refs in the
/// repository, none yet, under this server's capabilities).
fn serving(state: &mut ShardState) -> (RepositoryId, SocketAddrV4, Vec<u8>) {
  serving_within(state, ROOM, GIT_KEEPALIVE_NS)
}

/// [`serving`], each request admitted `room`, git's keepalive written after `keepalive_ns` of a silent fetch answer.
fn serving_within(
  state: &mut ShardState,
  room: u64,
  keepalive_ns: u64,
) -> (RepositoryId, SocketAddrV4, Vec<u8>) {
  let repository = git_repository(state, NAME);
  let mut advertised = Vec::new();
  ReceivePack::start(
    state,
    (&owner(), repository),
    true,
    &mut advertised,
    Unpresented,
  )
  .expect("the owner's push starts");
  let bounds = Bounds::of_shard(ONE_SEAT, ROOM_BYTES, TICK_NS);
  let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), bounds.backlog())
    .expect("a loopback listener");
  let address = listener.local_addr().expect("the listener's address");
  let edge =
    slates_rt::registry::with_current(|context| context.keep(EdgeState::new(bounds, HOST_SEED)))
      .expect("the shard's thread")
      .expect("the edge's state kept on its shard");
  slates_rt::futures::spawn(async move {
    let _ = serve(listener, edge, move || site_within(room, keepalive_ns)).await;
  })
  .and_then(slates_rt::futures::detach)
  .expect("the listener's task");
  (repository, address, advertised)
}

/// Opens a session, passes `protocol` as `GIT_PROTOCOL` when given (an `env` request before the command, as git's
/// client passes it), and asks the session to run [`COMMAND`]: whether the server accepted.
fn exec_asking(peer: &mut Peer, protocol: Option<&[u8]>) -> bool {
  exec_command(peer, protocol, COMMAND)
}

/// [`exec_asking`] for `command`.
fn exec_command(peer: &mut Peer, protocol: Option<&[u8]>, command: &[u8]) -> bool {
  let mut open = vec![90];
  put_string(&mut open, b"session");
  for word in [CLIENT_CHANNEL, CLIENT_WINDOW, CLIENT_PACKET] {
    open.extend_from_slice(&word.to_be_bytes());
  }
  peer.send(&open);
  let Some(confirmation) = peer.next(&[91, 92]) else {
    return false;
  };
  if confirmation.first() != Some(&91) {
    return false;
  }
  peer.channel = u32::from_be_bytes(confirmation[5..9].try_into().expect("a channel number"));
  if let Some(protocol) = protocol {
    let mut env = vec![98];
    env.extend_from_slice(&peer.channel.to_be_bytes());
    put_string(&mut env, b"env");
    env.push(0);
    put_string(&mut env, b"GIT_PROTOCOL");
    put_string(&mut env, protocol);
    peer.send(&env);
  }
  let mut request = vec![98];
  request.extend_from_slice(&peer.channel.to_be_bytes());
  put_string(&mut request, b"exec");
  request.push(1);
  put_string(&mut request, command);
  peer.send(&request);
  peer
    .next(&[99, 100])
    .and_then(|reply| reply.first().copied())
    == Some(99)
}

/// A whole push session with the held key: authenticate, run [`COMMAND`] asking for `protocol`, send `request` a
/// [`PIECE`] at a time (at once, before reading the advertisement: the edge holds it for the command, which reads it
/// only once it has advertised), and read to the end.
fn pushed_over_ssh(
  address: SocketAddrV4,
  protocol: Option<&[u8]>,
  request: &[u8],
) -> Option<Transcript> {
  let mut peer = Peer::connect(address)?;
  (query(&mut peer, &HELD_SEED)? == 60).then_some(())?;
  (sign(&mut peer, &HELD_SEED)? == 52).then_some(())?;
  exec_asking(&mut peer, protocol).then_some(())?;
  for piece in request.chunks(PIECE) {
    send_data(&mut peer, piece);
  }
  Some(transcript(&mut peer))
}

/// What a session that exits 0 with `out` on its standard output gave its client: the output, `exit-status`, EOF and
/// close, in order, and the connection ended.
fn exited_cleanly(out: Vec<u8>) -> Transcript {
  Transcript {
    out,
    err: Vec::new(),
    status: Some(0),
    order: vec![94, 98, 96, 97],
    ended: true,
  }
}

/// A-683 (§4.17.2 "A push" items 1 to 7 over "Transports" item 2; golden vectors: stock git 2.50.1's `create`
/// push, captured by `make-push-fixtures.sh`). Do: on an audit daemon's shard, make an empty git repository and serve
/// the SSH edge with the production site and services; connect with a key proving the repository's owner, run
/// `git-receive-pack '/pushed'`, send the request stock git sent to create `main` (its commands, then its pack) a few
/// bytes at a time, and read the session to its end. Expect: on standard output the advertisement the driver makes the
/// owner (no refs yet, this server's capabilities), then the very report `git receive-pack` wrote; nothing on standard
/// error; exit status 0, then EOF and close; and `main` then holding the fixture's first text, the tree the pushed
/// commit names.
#[test]
fn t_a683_stock_gits_push_over_ssh_lands_through_the_driver() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  crate::test_store::keep_landed(&daemon);
  let (repository, address, advertised) = observe_first(&daemon, serving);
  let [(create, _), ..] = chain();
  let pushed = pushed_over_ssh(address, None, create.request);
  let landed = observe_first(&daemon, move |state| files(state, repository, "main"));
  daemon.stop();
  let answered = [advertised.as_slice(), gits_report(&create)].concat();
  assert_eq!(pushed, Some(exited_cleanly(answered)));
  assert_eq!(landed, vec![("text".to_owned(), text(false))]);
}

/// A-683 (the session's protocols; git v2.50.1 `builtin/receive-pack.c` lines 2558-2577 and 2643-2649). Do:
/// with the edge served as above, run three sessions: one asking for no protocol (v0), one for `version=1`, one for
/// `version=2`; each sends a flush alone, git's push of nothing to update. Expect: each exits 0 after the advertisement
/// the driver makes the owner, preceded by a `version 1` line for v1 alone, v2 answered at v0 (receive-pack has no v2),
/// and nothing after it; and nothing landed: `main` holds no file.
#[test]
fn t_a683_each_protocol_is_advertised_as_gits_and_a_push_of_nothing_writes_nothing() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (repository, address, advertised) = observe_first(&daemon, serving);
  let mut flush = Vec::new();
  pktline::write(Packet::Flush, &mut flush).expect("a flush");
  let mut version_one = Vec::new();
  pktline::write(Packet::Data(b"version 1\n"), &mut version_one).expect("a line");
  let sessions: Vec<Option<Transcript>> = [
    None,
    Some(b"version=1".as_slice()),
    Some(b"version=2".as_slice()),
  ]
  .into_iter()
  .map(|protocol| pushed_over_ssh(address, protocol, &flush))
  .collect();
  let landed = observe_first(&daemon, move |state| files(state, repository, "main"));
  daemon.stop();
  let expected = vec![
    Some(exited_cleanly(advertised.clone())),
    Some(exited_cleanly([version_one, advertised.clone()].concat())),
    Some(exited_cleanly(advertised)),
  ];
  assert_eq!(sessions, expected);
  assert_eq!(landed, Vec::new());
}

/// A lender whose shard's state is never lent, as the daemon's [`ThisShard`] answers on a thread with no state, a
/// fenced shard, or a state lent already.
#[derive(Clone, Copy, Debug)]
struct NotLent;

impl ShardLender for NotLent {
  fn lend<R>(&mut self, _step: impl FnOnce(&mut ShardState) -> R) -> Option<R> {
    None
  }
}

/// A lender of the shard state the test holds already: inside [`observe_first`], where [`ThisShard`] would be a nested
/// borrow.
struct HeldState<'a>(&'a mut ShardState);

impl ShardLender for HeldState<'_> {
  fn lend<R>(&mut self, step: impl FnOnce(&mut ShardState) -> R) -> Option<R> {
    Some(step(self.0))
  }
}

/// `future`'s output, polled once: a receive-pack command's steps never wait.
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

/// `message` as git's `die` refuses: on standard error after `fatal: `, with status 128.
fn died(message: &str) -> Refused {
  Refused {
    stderr: format!("fatal: {message}\n").into_bytes(),
    exit_status: GIT_DIE_STATUS,
  }
}

/// A-683 (the receive-pack module's refusals; A-654 part 3's lender). Do: start a push at protocol v1 whose shard's
/// state its lender does not lend. Expect: its first step refused as the site refuses a service not served,
/// `fatal: git-receive-pack is not served yet` with git's `die` status, 128, where the HTTP site answers `503`.
#[test]
fn t_a683_a_push_whose_shard_is_not_lent_is_refused_as_not_served() {
  let mut command = ReceiveCommand::new(NotLent, owner(), (GONE, NAME.to_owned()), Version::V1);
  assert_eq!(
    ready(command.step(ClientGone::never())),
    Err(died("git-receive-pack is not served yet"))
  );
}

/// A-683 (the receive-pack module's refusals; git v2.50.1 `builtin/receive-pack.c` line 2547). Do: on an audit
/// daemon's shard, its state lent, start a push to a repository the shard's partition does not hold, as one destroyed
/// after the site found it. Expect: its first step refused in git's words for a path that is no repository,
/// `fatal: 'gone' does not appear to be a git repository`, with 128.
#[test]
fn t_a683_a_repository_gone_before_its_push_starts_is_refused_in_gits_words() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let refused = observe_first(&daemon, |state| {
    let mut command = ReceiveCommand::new(
      HeldState(state),
      owner(),
      (GONE, "gone".to_owned()),
      Version::V0,
    );
    ready(command.step(ClientGone::never()))
  });
  daemon.stop();
  assert_eq!(
    refused,
    Err(died("'gone' does not appear to be a git repository"))
  );
}

/// Shape: one request's room in [`a745_a_long_historys_clone_over_ssh_is_answered_within_one_requests_room`]: 64 KiB,
/// about one ring region of a shard's budget, far below the long history's pack.
const REQUEST_ROOM: u64 = 1 << 16;

/// A-745 (§4.17.1 "A fetch's answer": a request's room bounds a slice, never a repository; found by use with A-713:
/// git/git cut at v1.0.0 cloned over SSH was refused for its room). Do: on an audit daemon's shard, serve the SSH edge
/// with the production site, each request admitted [`REQUEST_ROOM`]; push a line of the HTTP edge's long history
/// (`vcs_http::daemon_edge_tests::LONG_HISTORY` commits) over SSH; then clone it over SSH as git's client does at protocol
/// v2 (`git-upload-pack` with `GIT_PROTOCOL=version=2`, a `fetch` of its last commit with `done`, then a flush), each
/// client waiting for its answer while its session is open, as git's does (a fixed patience failed it 3 of 3 on a
/// loaded machine, and 2 of 2 on the Mac Studio's efficiency cores, where the push answers in 6.1 s and the clone in
/// 18.1 s, both exiting 0). Expect:
/// the push exits 0; the clone exits 0, its answer after the advertisement a packfile section holding every object
/// pushed, in a pack larger than one request's room.
#[test]
fn a745_a_long_historys_clone_over_ssh_is_answered_within_one_requests_room() {
  use crate::vcs_http::daemon_edge_tests::{
    LONG_HISTORY, fetch_of, fetched_pack, ids_in, long_history, packets_from,
  };
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (_, address, _) = observe_first(&daemon, |state| {
    serving_within(state, REQUEST_ROOM, GIT_KEEPALIVE_NS)
  });
  let (request, last) = long_history(LONG_HISTORY);
  let pushed = pushed_patiently(address, &request);
  let fetched = fetched_over_ssh(address, &fetch_of(last));
  daemon.stop();
  assert_eq!(pushed.map(|transcript| transcript.status), Some(Some(0)));
  let fetched = fetched.expect("the clone's session");
  assert_eq!(
    fetched.status,
    Some(0),
    "{}",
    String::from_utf8_lossy(&fetched.err)
  );
  let (_, advertised_end) = packets_from(&fetched.out, 0);
  let answer = fetched.out.get(advertised_end..).unwrap_or_default();
  let pack = fetched_pack(answer);
  let (_, commands_end) = packets_from(&request, 0);
  let pushed_pack = request.get(commands_end..).expect("the push's pack");
  assert_eq!(ids_in(&pack).len(), ids_in(pushed_pack).len());
  assert!(
    u64::try_from(pack.len()).unwrap_or(u64::MAX) > REQUEST_ROOM,
    "a pack of {} bytes",
    pack.len()
  );
}

/// A-722's keepalive on the fetch over SSH (the HTTP edge's since 2ab402b95; GAPS: the SSH edge's upload wrote none, its
/// answer silent from its `packfile` section's head until its pack's first packet while the objects were chosen; git's
/// upload-pack writes `0005\1` while `pack-objects` is silent, git 2.50.1's `create_pack_file`). Do: on an audit
/// daemon's shard, serve the SSH edge with the production site, git's keepalive interval none; push a history of
/// `vcs_http::daemon_edge_tests::KEPT_ALIVE_HISTORY` commits over SSH; then clone it over SSH at protocol v2 (a `fetch`
/// of its last commit with `done`, then a flush). Expect: both sessions exit 0; the answer after the advertisement a
/// `packfile` section whose first packet is git's keepalive (an empty band-1 packet), its pack whole after the
/// keepalives, every object pushed.
#[test]
fn a_long_fetchs_answer_over_ssh_is_kept_alive_with_gits_keepalive_while_its_pack_is_chosen() {
  use crate::vcs_http::daemon_edge_tests::{
    KEPT_ALIVE_HISTORY, OPENING_SHOWN, fetch_of, fetched_pack, ids_in, long_history, packets_from,
  };
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (_, address, _) = observe_first(&daemon, |state| serving_within(state, ROOM, 0));
  let (request, last) = long_history(KEPT_ALIVE_HISTORY);
  let pushed = pushed_patiently(address, &request);
  let fetched = fetched_over_ssh(address, &fetch_of(last));
  daemon.stop();
  assert_eq!(pushed.map(|transcript| transcript.status), Some(Some(0)));
  let fetched = fetched.expect("the clone's session");
  assert_eq!(
    fetched.status,
    Some(0),
    "{}",
    String::from_utf8_lossy(&fetched.err)
  );
  let (_, advertised_end) = packets_from(&fetched.out, 0);
  let answer = fetched.out.get(advertised_end..).unwrap_or_default();
  let (lines, _) = packets_from(answer, 0);
  let opening: Vec<&[u8]> = lines
    .iter()
    .take(2)
    .map(|line| {
      line
        .get(..line.len().min(OPENING_SHOWN))
        .unwrap_or_default()
    })
    .collect();
  assert_eq!(opening, [b"packfile\n".as_slice(), [1].as_slice()]);
  let (_, commands_end) = packets_from(&request, 0);
  let pushed_pack = request.get(commands_end..).expect("the push's pack");
  assert_eq!(
    ids_in(&fetched_pack(answer)).len(),
    ids_in(pushed_pack).len()
  );
}

/// Goal 93 item 2b, lifted again (main 01a20182's r165 on git/git with the first lift: `clone`, `clone-ssh` and
/// `shallow` all read "possible repository corruption on the remote side", the `Unreadable` reaching the SSH edge and
/// the shallow path). Do: on an audit daemon's shard, serve the SSH edge with the production site; push over SSH a
/// create of `main` at a commit whose one file is `vcs_http::daemon_edge_tests::LET_GO_FILE` bytes, a pack of many
/// windows; with nothing let go by hand, wait until the shard counts landed bytes let go (by the commit's own let-go, or
/// first by the pack's home there); then clone `main` over SSH at protocol v2, whole and one commit deep. Expect: the
/// push exits 0; bytes let go; both clones exit 0, each pack the push's three objects, each the pushed bytes. Red with the
/// guard (`vcs_store::LANDED_LET_GO` false): nothing let go.
#[test]
#[ignore = "the landed let-go is withdrawn (vcs_store::LANDED_LET_GO false, main 01a20182 2026-11-23): a re-lift's red, run with it on"]
fn a_pushed_pack_let_go_by_its_commit_is_cloned_whole_and_shallow_over_ssh() {
  use crate::vcs_http::daemon_edge_tests::{
    LET_GO_FILE, fetch_of, fetched_pack, fetched_pack_after, large_create, objects_of,
    packets_from, shallow_fetch_of,
  };
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (_, address, _) = observe_first(&daemon, |state| {
    serving_within(state, ROOM, GIT_KEEPALIVE_NS)
  });
  let (request, commit) = large_create(LET_GO_FILE);
  let pushed = pushed_patiently(address, &request);
  let let_go_bytes = || {
    observe_first(&daemon, |state| {
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      count(crate::vcs_store::LANDED_LET_GO_BYTES)
        + count(crate::vcs_store::homes::LANDED_HOME_LET_GO_BYTES)
    })
  };
  let waited = std::time::Instant::now();
  while let_go_bytes() == 0 && waited.elapsed() < std::time::Duration::from_secs(10) {
    // The harness paces its looks; the daemon under test waits on its wheel.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(std::time::Duration::from_millis(20));
  }
  let let_go = let_go_bytes();
  let cloned = fetched_adjusting(address, &fetch_of(commit));
  let shallow = fetched_adjusting(address, &shallow_fetch_of(commit));
  daemon.stop();
  assert_eq!(pushed.map(|transcript| transcript.status), Some(Some(0)));
  assert!(
    let_go > 0,
    "the push's commit let its landed pack go: {let_go} bytes"
  );
  let (_, commands_end) = packets_from(&request, 0);
  let pushed_objects = objects_of(request.get(commands_end..).expect("the push's pack"));
  for (what, fetched, pack_of) in [
    ("clone", cloned, fetched_pack as fn(&[u8]) -> Vec<u8>),
    ("shallow clone", shallow, fetched_pack_after),
  ] {
    let fetched = fetched.unwrap_or_else(|| panic!("the {what}'s session"));
    assert_eq!(
      fetched.status,
      Some(0),
      "the {what}: {}",
      String::from_utf8_lossy(&fetched.err)
    );
    let (_, advertised_end) = packets_from(&fetched.out, 0);
    let answer = fetched.out.get(advertised_end..).unwrap_or_default();
    assert_eq!(
      objects_of(&pack_of(answer)),
      pushed_objects,
      "the {what} whole, each object the pushed bytes"
    );
  }
}

/// Main 01a20182's r227 (the re-lift on main, git/git over SSH: `fetch-pack: unexpected disconnect while reading
/// sideband packet` 114 KiB into the mirror fetch; on a made history, a shallow clone right after its `packfile`
/// header): an answer whose pack's choice reads an object in a let-go pack (here the shallow cut's reads of a later
/// push's commit and tree) is held for its window (`Answer::held_away`) with its sections already written; the SSH edge
/// dropped such an answer as written, so the session ended mid-answer, where the HTTP edge brings the window back and
/// makes the answer again past what it wrote. Do: on an audit daemon's shard, serve the SSH edge with the production
/// site; push over SSH `main` at a commit of one 256 KiB file, then a commit over it carrying a file and a tree of its
/// own, a small pack; with nothing let go by hand, wait for both pushes' commits to let their packs go; then fetch over
/// SSH as `git clone --depth 1` asks (`include-tag`, `deepen 1`, `want-ref refs/heads/main`, `done`). Expect: the session
/// exits 0, its pack the later commit's objects, each the pushed bytes. A guard, green before and after the fix: this
/// single-shard edge does not reach the held answer that stock git's own session reached (git_interop's whole session
/// over SSH on a made history, red 3 of 3 before the fix: the shallow clone's answer held for a window of the work
/// clone's 284-byte pack); the unit red at the edge is owed.
#[test]
#[ignore = "the landed let-go is withdrawn (vcs_store::LANDED_LET_GO false, main 01a20182 2026-11-23): a re-lift's red, run with it on"]
fn a_shallow_clone_of_a_later_push_let_go_is_answered_whole_over_ssh() {
  use crate::vcs_http::daemon_edge_tests::{
    fetched_pack_after, git_name, large_create, objects_of, packets_from, stored_pack,
  };
  use slates_git::object_name::Kind;
  use slates_git::pktline::{self, Packet};
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (_, address, _) = observe_first(&daemon, |state| {
    serving_within(state, ROOM, GIT_KEEPALIVE_NS)
  });
  let let_go_bytes = || {
    observe_first(&daemon, |state| {
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      count(crate::vcs_store::LANDED_LET_GO_BYTES)
        + count(crate::vcs_store::homes::LANDED_HOME_LET_GO_BYTES)
    })
  };
  let let_go_past = |past: u64| {
    let waited = std::time::Instant::now();
    while let_go_bytes() <= past && waited.elapsed() < std::time::Duration::from_secs(10) {
      // The harness paces its looks; the daemon under test waits on its wheel.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let_go_bytes()
  };
  let (base, base_commit) = large_create(1 << 18);
  let first = pushed_patiently(address, &base);
  let after_first = let_go_past(0);
  let file = b"carried\n".to_vec();
  let mut tree = b"100644 carried\0".to_vec();
  tree.extend_from_slice(git_name(Kind::Blob, &file).as_bytes());
  let tree_id = git_name(Kind::Tree, &tree);
  let commit = format!(
    "tree {tree_id}\nparent {base_commit}\nauthor A <a@example.com> 1700000001 +0000\n\
     committer A <a@example.com> 1700000001 +0000\n\ncarried\n"
  )
  .into_bytes();
  let commit_id = git_name(Kind::Commit, &commit);
  let pack = stored_pack(&[
    (Kind::Commit, commit.as_slice()),
    (Kind::Tree, tree.as_slice()),
    (Kind::Blob, file.as_slice()),
  ]);
  let mut on_top = Vec::new();
  let line = format!("{base_commit} {commit_id} refs/heads/main\0 report-status");
  pktline::write(Packet::Data(line.as_bytes()), &mut on_top).expect("the command");
  pktline::write(Packet::Flush, &mut on_top).expect("the flush");
  on_top.extend_from_slice(&pack);
  let second = pushed_patiently(address, &on_top);
  let after_second = let_go_past(after_first);
  let mut fetch = Vec::new();
  pktline::write(Packet::Data(b"command=fetch\n"), &mut fetch).expect("the command");
  pktline::write(Packet::Delim, &mut fetch).expect("the delimiter");
  for line in [
    "thin-pack\n",
    "no-progress\n",
    "include-tag\n",
    "ofs-delta\n",
    "deepen 1\n",
    "want-ref refs/heads/main\n",
    "done\n",
  ] {
    pktline::write(Packet::Data(line.as_bytes()), &mut fetch).expect("an argument");
  }
  pktline::write(Packet::Flush, &mut fetch).expect("the flush");
  let fetched = fetched_adjusting(address, &fetch);
  daemon.stop();
  assert_eq!(first.map(|transcript| transcript.status), Some(Some(0)));
  assert_eq!(second.map(|transcript| transcript.status), Some(Some(0)));
  assert!(
    after_first > 0 && after_second > after_first,
    "both pushes' packs let go: {after_first} then {after_second} bytes"
  );
  let fetched = fetched.expect("the fetch's session");
  assert_eq!(
    fetched.status,
    Some(0),
    "{}",
    String::from_utf8_lossy(&fetched.err)
  );
  let (_, advertised_end) = packets_from(&fetched.out, 0);
  let answer = fetched.out.get(advertised_end..).unwrap_or_default();
  assert_eq!(
    objects_of(&fetched_pack_after(answer)),
    objects_of(&pack),
    "the pack the later commit's objects, each the pushed bytes"
  );
}

/// A-855 on the SSH edge (main 01a20182's order of 2026-11-23: a fetch past its room waits for room and is answered,
/// never refused while room may come; the HTTP edge's `Upload::wait_for_room` had no SSH twin). Do: on an audit daemon's
/// shard, serve the SSH edge with the production site; push over SSH `vcs_http::daemon_edge_tests::tagged_history` of a
/// few commits; make the next rooms the repository's shard gives a session a wide one for its advertisement, then the
/// fetch request's own length for its fetch (room for its body, none for its answer), then the shard's own; fetch every
/// ref whole over SSH. Expect: the session exits 0, its
/// pack every object pushed, no abort. Red before: the fetch refused in its first room, git's die status.
#[test]
fn a_fetch_past_its_room_over_ssh_waits_for_more_and_is_answered_whole() {
  use crate::vcs_http::daemon_edge_tests::{
    TAGGED_FEW, fetched_pack_after, ids_in, packets_from, tagged_history, whole_fetch_of,
  };
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (_, address, _) = observe_first(&daemon, |state| {
    serving_within(state, ROOM, GIT_KEEPALIVE_NS)
  });
  let (request, wants, objects) = tagged_history(TAGGED_FEW);
  let pushed = pushed_patiently(address, &request);
  let fetch = whole_fetch_of(&wants);
  let short = u64::try_from(fetch.len()).expect("a length");
  // The session's advertisement is admitted first (a wide room), then its fetch the short one.
  observe_first(&daemon, move |_| {
    crate::vcs_store::upload::ROOMS.with(|rooms| {
      let mut rooms = rooms.borrow_mut();
      rooms.push_back(1 << 30);
      rooms.push_back(short);
    });
  });
  let fetched = fetched_adjusting(address, &fetch);
  let again = observe_first(&daemon, |state| {
    state
      .refusals
      .get(crate::vcs_http::git::upload::ANSWERED_AGAIN)
      .copied()
      .unwrap_or(0)
  });
  daemon.stop();
  assert_eq!(pushed.map(|transcript| transcript.status), Some(Some(0)));
  let fetched = fetched.expect("the fetch's session");
  assert_eq!(
    fetched.status,
    Some(0),
    "{}",
    String::from_utf8_lossy(&fetched.err)
  );
  assert!(again >= 1, "the answer made again within the wider room");
  let (_, advertised_end) = packets_from(&fetched.out, 0);
  let answer = fetched.out.get(advertised_end..).unwrap_or_default();
  assert!(
    !answer
      .windows(b"aborting".len())
      .any(|window| window == b"aborting"),
    "no abort"
  );
  assert_eq!(ids_in(&fetched_pack_after(answer)).len(), objects);
}

/// [`fetched_over_ssh`], read as OpenSSH's client reads: the channel's window given back
/// (`SSH_MSG_CHANNEL_WINDOW_ADJUST`) once half of it is read, so an answer past [`CLIENT_WINDOW`] is read whole (as
/// `vcs_ssh::tests::hg`'s `transcript_adjusting` reads).
fn fetched_adjusting(address: SocketAddrV4, request: &[u8]) -> Option<Transcript> {
  let mut peer = Peer::connect_patient(address)?;
  (query(&mut peer, &HELD_SEED)? == 60).then_some(())?;
  (sign(&mut peer, &HELD_SEED)? == 52).then_some(())?;
  exec_command(&mut peer, Some(b"version=2"), b"git-upload-pack '/pushed'").then_some(())?;
  for piece in request.chunks(PIECE) {
    send_data(&mut peer, piece);
  }
  send_data(&mut peer, b"0000");
  Some(transcript_giving_the_window_back(&mut peer))
}

/// Reads the session as OpenSSH's client does, at the wire's pace: each data message's bytes counted, the window given
/// back (`SSH_MSG_CHANNEL_WINDOW_ADJUST`) once half of it is read, until the server closes the session; everything it
/// gave.
fn transcript_giving_the_window_back(peer: &mut Peer) -> Transcript {
  let mut transcript = Transcript::default();
  let mut unadjusted: u32 = 0;
  while let Some(message) = peer.next(&[94, 95, 96, 97, 98]) {
    let number = message[0];
    if transcript.order.last() != Some(&number) {
      transcript.order.push(number);
    }
    let string = |at: usize| {
      let len = u32::from_be_bytes(message[at..at + 4].try_into().expect("a length")) as usize;
      message[at + 4..at + 4 + len].to_vec()
    };
    match number {
      94 | 95 => {
        let data = if number == 94 { string(5) } else { string(9) };
        if number == 94 {
          transcript.out.extend_from_slice(&data);
        } else {
          transcript.err.extend_from_slice(&data);
        }
        unadjusted += u32::try_from(data.len()).unwrap();
        if unadjusted >= CLIENT_WINDOW / 2 {
          let mut adjust = vec![93];
          adjust.extend_from_slice(&peer.channel.to_be_bytes());
          adjust.extend_from_slice(&unadjusted.to_be_bytes());
          peer.send(&adjust);
          unadjusted = 0;
        }
      }
      98 if string(5) == b"exit-status" => {
        let at = 5 + 4 + b"exit-status".len() + 1;
        transcript.status = Some(u32::from_be_bytes(
          message[at..at + 4].try_into().expect("four bytes"),
        ));
      }
      97 => {
        let mut close = vec![97];
        close.extend_from_slice(&peer.channel.to_be_bytes());
        peer.send(&close);
        transcript.ended = peer.ended();
        break;
      }
      _ => {}
    }
  }
  transcript
}

/// [`pushed_over_ssh`] at protocol v0, its client patient: it waits for the push's answer while the session is open,
/// however long the push's work takes on a loaded machine (`Peer::connect_patient`).
fn pushed_patiently(address: SocketAddrV4, request: &[u8]) -> Option<Transcript> {
  let mut peer = Peer::connect_patient(address)?;
  (query(&mut peer, &HELD_SEED)? == 60).then_some(())?;
  (sign(&mut peer, &HELD_SEED)? == 52).then_some(())?;
  exec_asking(&mut peer, None).then_some(())?;
  for piece in request.chunks(PIECE) {
    send_data(&mut peer, piece);
  }
  Some(transcript(&mut peer))
}

/// A clone session with the held key: authenticate, run `git-upload-pack '/pushed'` at protocol v2, send `request` and a
/// flush that ends the session, and read to the end, patient as [`pushed_patiently`] is.
fn fetched_over_ssh(address: SocketAddrV4, request: &[u8]) -> Option<Transcript> {
  let mut peer = Peer::connect_patient(address)?;
  (query(&mut peer, &HELD_SEED)? == 60).then_some(())?;
  (sign(&mut peer, &HELD_SEED)? == 52).then_some(())?;
  exec_command(&mut peer, Some(b"version=2"), b"git-upload-pack '/pushed'").then_some(())?;
  for piece in request.chunks(PIECE) {
    send_data(&mut peer, piece);
  }
  send_data(&mut peer, b"0000");
  Some(transcript(&mut peer))
}
