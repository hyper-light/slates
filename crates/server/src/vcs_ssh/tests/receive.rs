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
use crate::daemon::{audit_daemon, observe_first};
use crate::git_push::tests::{chain, files, git_repository, gits_report, owner, text};
use crate::git_push::{ReceivePack, Unpresented};
use crate::state::ShardState;
use crate::vcs_http::Bounds;
use crate::vcs_http::ShardRepositories;
use crate::vcs_http::git::{ShardLender, ThisShard};
use crate::vcs_ssh::receive::ReceiveCommand;
use crate::vcs_ssh::site::{Command, GIT_DIE_STATUS, Refused, ToolServices, ToolSite};
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
/// connection runs on ([`ThisShard`]) and admitted [`ROOM`].
fn site() -> ToolSite<OwnerKeys, ShardRepositories, DaemonServices> {
  ToolSite::new(
    OwnerKeys,
    ShardRepositories,
    ToolServices::new(ShardStores, ServedViews, ThisShard, ROOM),
  )
}

/// The daemon's services ([`site`]).
type DaemonServices = ToolServices<ShardStores, ServedViews, ThisShard>;

/// Makes git repository [`NAME`] on the shard and serves the edge there with [`site`], its listener on a loopback port:
/// the repository, the listener's address, and the advertisement the driver makes the owner now (git's refs in the
/// repository, none yet, under this server's capabilities).
fn serving(state: &mut ShardState) -> (RepositoryId, SocketAddrV4, Vec<u8>) {
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
    let _ = serve(listener, edge, site).await;
  })
  .and_then(slates_rt::futures::detach)
  .expect("the listener's task");
  (repository, address, advertised)
}

/// Opens a session, passes `protocol` as `GIT_PROTOCOL` when given (an `env` request before the command, as git's
/// client passes it), and asks the session to run [`COMMAND`]: whether the server accepted.
fn exec_asking(peer: &mut Peer, protocol: Option<&[u8]>) -> bool {
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
  put_string(&mut request, COMMAND);
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
  let daemon = audit_daemon();
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
  let daemon = audit_daemon();
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
    ready(command.step()),
    Err(died("git-receive-pack is not served yet"))
  );
}

/// A-683 (the receive-pack module's refusals; git v2.50.1 `builtin/receive-pack.c` line 2547). Do: on an audit
/// daemon's shard, its state lent, start a push to a repository the shard's partition does not hold, as one destroyed
/// after the site found it. Expect: its first step refused in git's words for a path that is no repository,
/// `fatal: 'gone' does not appear to be a git repository`, with 128.
#[test]
fn t_a683_a_repository_gone_before_its_push_starts_is_refused_in_gits_words() {
  let daemon = audit_daemon();
  let refused = observe_first(&daemon, |state| {
    let mut command = ReceiveCommand::new(
      HeldState(state),
      owner(),
      (GONE, "gone".to_owned()),
      Version::V0,
    );
    ready(command.step())
  });
  daemon.stop();
  assert_eq!(
    refused,
    Err(died("'gone' does not appear to be a git repository"))
  );
}
