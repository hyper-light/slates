//! What the SSH edge serves (A-651; §4.17.2 "Transports" item 2, "Access"; §4.17.3; §4.17.4's svn+ssh, piece 12): a key
//! proved to a principal of §4.13 through the site's lookup ([`SshSite::keys`], `slates_ssh`'s `KeyHolders`, then
//! [`SshSite::lookup`]), and each `exec` dispatched by its tool ([`SshSite::dispatch`]) to the command the edge then
//! runs ([`Command`]). The lookup is the one record of principals' credentials both edges read (`crate::credentials`,
//! A-625 part 3): [`RecordKeys`] leaves every key to it, and [`ToolSite`] answers by [`key_holder`] (A-651 part 3).
//!
//! **Where an `exec` is decided** ([`SshSite::locate`], A-651 part 4). A command that names a repository is decided on
//! the shard whose partition its name routes to (D-14), as the HTTP edge decides a head: the edge moves the connection
//! there first (`super`'s module doc), so the repository's record, its policy and its store are read on their own shard.
//!
//! **The production site** ([`ToolSite`]) decides an `exec` in A-625 part 2's order for HTTP, each refusal written to the
//! client's standard error with its tool's own wording and exit status, as the tool's own program would refuse it:
//! 1. The repository, for git and Mercurial, whose commands name it: the one whose name is the path as the client sent
//!    it, without one leading slash, and of the command's tool; none is refused as the tool refuses a missing repository
//!    (git: `fatal: '<path>' does not appear to be a git repository`, its `die`'s 128 [C: git `builtin/upload-pack.c`,
//!    `usage.c` at `e1ec09b2`]; Mercurial: `abort: repository <path> not found`, 255 [C: Mercurial 7.2 `localrepo.py`,
//!    `scmutil.py`'s `callcatch`, `dispatch.py`'s `sys.exit(status & 255)`]). No second spelling is tried. Before it,
//!    a Mercurial path that starts `--` is refused as 7.2's dispatch refuses it before opening anything, naming the
//!    arguments as `stringutil.pprint` writes them, each in single quotes with its bytes as Python's
//!    `codecs.escape_encode` writes them: `abort: potentially unsafe serve --stdio invocation: ['-R', '<path>',
//!    'serve', '--stdio']`, `'--hidden'` last when asked, 255; a path that starts with one `-` goes on to the
//!    repository's lookup, as in 7.2 [C: 7.2 `dispatch.py` `_runcatchfunc`; recorded from stock 7.2 on 2026-11-19
//!    (A-679): `--`, `--x`, quotes, tab, line ends, a backslash, DEL, a control byte and bytes past ASCII].
//! 2. The policy: a principal the repository's policy lets reach it for none of the command's rights is refused
//!    (`crate::vcs_policy::reaches`, A-597). upload-pack asks to read; receive-pack to submit, rewrite or delete;
//!    Mercurial's `serve --stdio`, which pulls and pushes in one session, any of the four, each command then judged by
//!    the session. A damaged policy is refused, never judged without it.
//! 3. The service ([`Services`]): one the server cannot start is refused as not served ([`Unserved`]), as the HTTP edge
//!    answers `503`. git's upload-pack runs (`super::git`, A-651 part 2) when the store lender holds the repository's
//!    store (A-625 part 3's [`StoreLender`]) at protocol v2 (`super::git`'s protocols): the daemon's lender lends each
//!    store from A-662's map on its owner shard (`crate::vcs_store::upload::ShardStores`, A-682). Mercurial's `serve
//!    --stdio` runs (`super::hg`, A-679: `slates_hg::serve::stdio::Stdio`, A-650 part 2) when the lender the HTTP site
//!    reads Mercurial's stores from holds the repository's ([`HgStores`]): the daemon's lends each repository's served
//!    view from its store on its owner shard (`crate::vcs_store::hg_view::ServedViews`, A-680 part 2a). The daemon
//!    hands this site both lenders once it starts the SSH edge (A-684). git's receive-pack runs A-654 part 2's driver
//!    over the repository's store on its owner shard, the shard's own (A-662), so it needs no store lender: each of its
//!    steps is lent the shard's state, as the HTTP site's are (A-654 part 3's [`ShardLender`]; `super::receive`,
//!    A-683). Each becomes a [`Command`] behind [`Services`] as it can run, and the not-served answer goes with the last
//!    of them.
//!
//! Subversion's `svnserve -t` names no repository: its client names one in the greeting's URL (`svn_ra_svn` protocol), so
//! its repository and policy are the tunnel session's to decide, and the edge only starts it (svnserve's own refusals
//! begin `svnserve: ` and exit `EXIT_FAILURE` [C: `svnserve.c` at `d434dc14`]). The tunnel runs as a service of its own
//! beside the others (`super::svn`'s `SvnServices`, A-688): A-656's connection over the channel, the repository found
//! and judged when its URL comes, its store lent as git's is, and refused as not served where it is not held here.

use std::cell::RefCell;

use slates_db::catalog::{CredentialKind, CredentialRecord, Principal, RepositoryRecord, VcsTool};
use slates_rt::tcp::TcpStream;
use slates_ssh::command::Command as Asked;
use slates_ssh::userauth::{Holding, KeyHolders, UserKey};
use slates_vfs::clock::Clock;

use super::git::UploadCommand;
use super::hg::HgCommand;
use super::receive::ReceiveCommand;
use crate::vcs_http::git::{Located, Repositories, ShardLender, StoreLender};
use crate::vcs_http::hg::HgStores;
use crate::vcs_policy::{self, Right};

/// Format: the refusal count of a key no credential in the record has the id of (A-651 part 3).
const KEY_UNKNOWN: &str = "vcs_ssh.key_unknown";
/// Format: the refusal count of a key whose id names a credential that is not this key: an HTTP token's, or a key of
/// other bytes.
const KEY_NOT_THIS: &str = "vcs_ssh.key_not_this_key";
/// Format: the refusal count of a key whose credential expired on the wall clock.
const KEY_EXPIRED: &str = "vcs_ssh.key_expired";
/// Format: the refusal count of a key whose credential proves a consumer that was revoked or holds no enrollment
/// (§4.13).
const KEY_REVOKED: &str = "vcs_ssh.key_revoked";
/// Format: the refusal count of a key the record gave no answer for: no shard, or its partition's shard silent within
/// its budget. The client is answered as for a key nobody holds, and may offer another or try again.
const KEY_UNANSWERED: &str = "vcs_ssh.key_unanswered";
/// Format: the distinct keys a principal the record proves signs with: its one key, since the record keeps no policy
/// asking for more (§4.13).
const KEYS_REQUIRED: u32 = 1;

/// Format: the status git's `die` exits with (`usage.c` at `e1ec09b2`: `exit(128)`), the one git-upload-pack and
/// git-receive-pack give a repository they cannot enter.
pub(crate) const GIT_DIE_STATUS: u32 = 128;
/// Format: the status `hg` exits with on an abort in Mercurial 7.2: `callcatch`'s coarse code, -1 (`scmutil.py`), through
/// `sys.exit(status & 255)` (`dispatch.py`).
pub(crate) const HG_ABORT_STATUS: u32 = 255;
/// Format: svnserve's failure, `EXIT_FAILURE` (`svnserve.c` at `d434dc14`; C's `stdlib.h`).
pub(crate) const SVNSERVE_FAILURE_STATUS: u32 = 1;
/// Format: how a Mercurial repository argument begins that 7.2's dispatch refuses in a `serve --stdio`
/// (`dispatch.py` `_runcatchfunc`: it prohibits "any repo name that starts with '--'").
const HG_UNSAFE_START: &[u8] = b"--";

/// A refused or failed command: what its client reads on standard error, and its exit status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refused {
  /// The message.
  pub(crate) stderr: Vec<u8>,
  /// The exit status.
  pub(crate) exit_status: u32,
}

/// What a command waits for after a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
  /// It can step again: ask it.
  More,
  /// It needs more of its client's input before it steps again.
  Input,
  /// It exited with this status, once its output is sent.
  Exited(u32),
}

/// What one step of a command did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stepped {
  /// The client's input it consumed, which the edge gives back to the client's window.
  pub(crate) consumed: usize,
  /// What it waits for now.
  pub(crate) next: Next,
}

/// Whether the connection's client has gone, as a command waits on something of its own within a step: a hook's run
/// (git's receive-pack, A-683; Subversion's tunnel, A-730), as the HTTP edge's `crate::vcs_http::ClientGone` tells a
/// service once its request's body has ended (A-665 part 4a). The connection's socket is watched for the client's close
/// while the step runs and the edge reads nothing else.
///
/// An SSH client may still send while it waits: a window adjustment, its channel's EOF or close, a keepalive or a
/// disconnect, each an encrypted packet this watch cannot read. So it takes the client's bytes rather than peeking at
/// them, as the HTTP edge's watch peeks (there the client sends nothing once its body has ended): each run as it arrives
/// (`read_now` once readability says something came; the wait holds no buffer, so a step that drops the watch loses
/// none of them), kept in `kept`, which the edge hands to its session after the step as it would have read them then.
/// So the client's end of stream or a reset, behind whatever it sent, ends the wait. Past `room` bytes kept (the one
/// read a connection holds beside its session, `super::READ_BYTES`), the watch reads no more and asks the kernel's own
/// record of the connection once (`TcpStream::peer_closed`): left is gone; still there leaves it unknown, and the wait
/// does not end on it.
#[derive(Clone, Copy)]
pub(crate) struct ClientGone<'s> {
  /// The connection's socket, the client's bytes taken while it is watched, and how many may be kept; none for a caller
  /// with no client (the tests' commands stepped directly).
  watched: Option<(&'s TcpStream, &'s RefCell<Vec<u8>>, usize)>,
}

/// What one look at a watched socket found ([`ClientGone`]).
enum Looked {
  /// Its end of stream, or the socket refused (a reset).
  Gone,
  /// The client's bytes, kept; or nothing queued after all.
  Taken,
  /// The bytes kept reached their room: nothing more is read.
  Full,
}

impl<'s> ClientGone<'s> {
  /// The client of the connection over `stream`, its bytes kept in `kept` up to `room`.
  pub(crate) fn watching(stream: &'s TcpStream, kept: &'s RefCell<Vec<u8>>, room: usize) -> Self {
    Self {
      watched: Some((stream, kept, room)),
    }
  }

  /// No client to watch: never gone.
  #[cfg(test)]
  pub(crate) fn never() -> Self {
    Self { watched: None }
  }

  /// Resolves once the client has gone (the type's doc); never while it is there.
  pub(crate) async fn gone(self) {
    let Some((stream, kept, room)) = self.watched else {
      return core::future::pending().await;
    };
    loop {
      if stream.wait_readable().await.is_err() {
        return;
      }
      match look(stream, kept, room) {
        Looked::Gone => return,
        Looked::Taken => {}
        Looked::Full => {
          if stream.peer_closed().unwrap_or(true) {
            return;
          }
          // Still there, with bytes this watch may not take: unknown (the type's doc).
          return core::future::pending().await;
        }
      }
    }
  }
}

/// Takes what is queued on `stream` into `kept`, up to `room` bytes kept, without waiting ([`ClientGone`]): no await
/// runs while `kept` is borrowed.
fn look(stream: &TcpStream, kept: &RefCell<Vec<u8>>, room: usize) -> Looked {
  let mut kept = kept.borrow_mut();
  let held = kept.len();
  let free = room.saturating_sub(held);
  if free == 0 {
    return Looked::Full;
  }
  kept.resize(held.saturating_add(free), 0);
  let read = stream.read_now(kept.get_mut(held..).unwrap_or_default());
  let taken = match read {
    Ok(Some(count)) => count,
    Ok(None) | Err(_) => 0,
  };
  kept.truncate(held.saturating_add(taken));
  match read {
    Ok(Some(0)) | Err(_) => Looked::Gone,
    Ok(Some(_)) | Ok(None) => Looked::Taken,
  }
}

/// A command an `exec` started, sans-io as each tool's entry point is (Mercurial's `serve::stdio::Stdio`, A-650 part 2;
/// git's `upload::serve`, A-640 part 2): the client's input held as it arrives, up to the channel's window, until a step
/// consumes it; its standard error and output held until the channel takes them, error first.
pub(crate) trait Command {
  /// Takes one run of the client's input, held until a step consumes it; refused past what the command holds, the
  /// channel's window.
  fn input(&mut self, run: &[u8]) -> Result<(), Refused>;
  /// The client's input has ended.
  fn input_end(&mut self);
  /// Serves what it can: what it consumed of its input, and what it waits for now. A step that waits on something of
  /// its own (a hook's run) ends that wait once `gone` says its client has gone.
  async fn step(&mut self, gone: ClientGone<'_>) -> Result<Stepped, Refused>;
  /// What it has written and the channel has not taken: standard error, then standard output.
  fn output(&self) -> (&[u8], &[u8]);
  /// The channel took `stderr` bytes of its standard error and `stdout` of its standard output.
  fn sent(&mut self, stderr: usize, stdout: usize);
}

/// How a site answers an `exec`.
pub(crate) enum Dispatched<C> {
  /// Run this command.
  Run(C),
  /// Refuse it: the message and the status.
  Refuse(Refused),
}

/// What serves an edge's connections: whose keys prove which principals, and what each `exec` runs.
pub(crate) trait SshSite {
  /// The lookup a connection's authentication asks (§4.13's principals).
  type Keys: KeyHolders<Principal = Principal>;
  /// The command a dispatched `exec` runs.
  type Command: Command;
  /// The lookup for one connection.
  fn keys(&mut self) -> Self::Keys;
  /// Who holds `key` (its blob `blob`) for `user`, when the connection's lookup could not say yet
  /// (`Holding::Unknown`): the record's answer, looked up where the record lives (A-651 part 3).
  async fn lookup(&mut self, user: &[u8], key: &UserKey, blob: &[u8]) -> Holding<Principal>;
  /// Where the repository `command` names lives: here, or the runtime shard its connection moves to, so the `exec` is
  /// dispatched on the shard that holds the repository (A-651 part 4). A command that names none is decided here.
  fn locate(&self, command: &Asked) -> Located;
  /// Dispatches `command` for `principal`, with the `GIT_PROTOCOL` value sent before it.
  fn dispatch(
    &mut self,
    principal: &Principal,
    command: &Asked,
    git_protocol: Option<&[u8]>,
  ) -> Dispatched<Self::Command>;
}

/// The production key holders (A-651 part 3; main's ruling under §4.13): a person enrolls a key through slates' own CLI
/// on the control channel, and the credential record keeps it on the partition its id routes to (`crate::credentials`,
/// A-625 part 3), which a session's synchronous ask cannot wait for. So every key is answered "not yet"
/// (`Holding::Unknown`), and the session waits while the site looks it up ([`ToolSite`]'s [`SshSite::lookup`]). Nothing
/// stands in for the record: no key file, no default key.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RecordKeys;

impl KeyHolders for RecordKeys {
  type Principal = Principal;

  fn holding(&mut self, _user: &[u8], _key: &UserKey, _blob: &[u8]) -> Holding<Principal> {
    Holding::Unknown
  }
}

/// Why the credential record proves no principal for a key a client presents (A-651 part 3), each counted apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyUnproven {
  /// No credential has the key's id.
  Unknown,
  /// The credential the id names is not this key: an HTTP token's, or a key of other bytes.
  NotThisKey,
  /// The credential expired on the wall clock.
  Expired,
  /// It proves a consumer that was revoked, or holds no enrollment (§4.13: "every later effect refuses").
  Revoked,
  /// The record gave no answer: no shard, or the partition's shard silent within its budget.
  Unanswered,
}

impl KeyUnproven {
  /// The refusal count it is counted under.
  fn counter(self) -> &'static str {
    match self {
      KeyUnproven::Unknown => KEY_UNKNOWN,
      KeyUnproven::NotThisKey => KEY_NOT_THIS,
      KeyUnproven::Expired => KEY_EXPIRED,
      KeyUnproven::Revoked => KEY_REVOKED,
      KeyUnproven::Unanswered => KEY_UNANSWERED,
    }
  }
}

/// Who `record` proves when a client presents the key `blob` at wall-clock instant `now_ns`: its principal, when it
/// holds an SSH key of exactly these bytes and is live (`CredentialRecord::live_at`; main's recipe); why not, otherwise.
/// A record's id is its blob's SHA-256, so other bytes under the id are a token's record or a collision, refused either
/// way.
pub(crate) fn key_holder(
  record: &CredentialRecord,
  blob: &[u8],
  now_ns: u64,
) -> Result<Principal, KeyUnproven> {
  match &record.kind {
    CredentialKind::SshKey { blob: held, .. } if held.as_slice() == blob => {}
    _ => return Err(KeyUnproven::NotThisKey),
  }
  if !record.live_at(now_ns) {
    return Err(KeyUnproven::Expired);
  }
  Ok(record.principal.clone())
}

/// The principal the credential record proves for the key `blob` (the module doc): its record looked up on the
/// partition the key's id routes to, from any shard (`crate::credentials::find`), decided by [`key_holder`] at the wall
/// clock's now, and, for a consumer's key, the consumer's enrollment asked of its own partition, as the HTTP edge asks
/// it (`crate::credentials::consumer_revoked`; §4.13: "every later effect refuses").
async fn recorded_holder(blob: &[u8]) -> Result<Principal, KeyUnproven> {
  let record = crate::credentials::find(crate::credentials::ssh_key_id(blob))
    .await
    .map_err(|_| KeyUnproven::Unanswered)?
    .ok_or(KeyUnproven::Unknown)?;
  let now_ns = crate::state::with_state(|state| state.clock.wall_ns())
    .and_then(|ns| u64::try_from(ns).ok())
    .ok_or(KeyUnproven::Unanswered)?;
  let principal = key_holder(&record, blob, now_ns)?;
  if let Principal::Consumer { consumer, .. } = &principal
    && crate::credentials::consumer_revoked(*consumer)
      .await
      .map_err(|_| KeyUnproven::Unanswered)?
  {
    return Err(KeyUnproven::Revoked);
  }
  Ok(principal)
}

/// A service an `exec` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Service {
  /// git's fetch and clone.
  GitUploadPack,
  /// git's push.
  GitReceivePack,
  /// Mercurial's `serve --stdio`, with `--hidden` when asked.
  Mercurial {
    /// Whether hidden changesets are served.
    hidden: bool,
  },
  /// Subversion's `svnserve -t`.
  Subversion,
}

impl Service {
  /// The service as its client names it, for a refusal's message.
  fn name(self) -> &'static str {
    match self {
      Service::GitUploadPack => "git-upload-pack",
      Service::GitReceivePack => "git-receive-pack",
      Service::Mercurial { .. } => "hg serve --stdio",
      Service::Subversion => "svnserve -t",
    }
  }
}

/// What a dispatched service starts with.
pub(crate) struct Start<'a> {
  /// The service.
  pub(crate) service: Service,
  /// The repository its command named; none for Subversion's tunnel.
  pub(crate) repository: Option<&'a RepositoryRecord>,
  /// Who asks.
  pub(crate) principal: &'a Principal,
  /// The `GIT_PROTOCOL` value, for git's services.
  pub(crate) git_protocol: Option<&'a [u8]>,
}

/// A service whose server half has not landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unserved;

/// The services behind the site.
pub(crate) trait Services {
  /// The command a started service runs.
  type Command: Command;
  /// Starts the service `start` names, or says it is not served.
  fn start(&mut self, start: &Start<'_>) -> Result<Self::Command, Unserved>;
}

/// The services as the server has them today: none has its server half, so each is [`Unserved`] (the module doc).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NotYet;

/// The services the server runs today: git's upload-pack from the stores `lender` lends (`super::git`, A-651 part 2),
/// Mercurial's `serve --stdio` from the stores `hg` lends (`super::hg`, A-679), and git's receive-pack through A-654
/// part 2's driver on the repository's owner shard, whose state `shard` lends a step at a time as it lends the HTTP
/// site's (`super::receive`, A-683), git's requests and pushes each admitted what its repository's shard could admit
/// (A-713, A-704) and Mercurial's requests `room` bytes; Subversion's tunnel is served
/// beside these by `super::svn`'s `SvnServices` (A-688). A repository whose store its tool's lender does not hold is not
/// served upload-pack or `serve --stdio`.
#[derive(Clone, Debug)]
pub(crate) struct ToolServices<L, H, S> {
  /// Where git's stores are lent.
  lender: L,
  /// Where Mercurial's stores are lent: the lender the HTTP site reads them from.
  hg: H,
  /// Where the shard's state is lent, for receive-pack (in the daemon the HTTP site's `ThisShard`).
  shard: S,
  /// The bytes each Mercurial request may hold: the edge's room.
  room: u64,
}

impl<L, H, S> ToolServices<L, H, S> {
  /// The services over `lender`'s git stores, `hg`'s Mercurial stores and the state `shard` lends, each Mercurial
  /// request admitted `room` bytes.
  pub(crate) fn new(lender: L, hg: H, shard: S, room: u64) -> Self {
    Self {
      lender,
      hg,
      shard,
      room,
    }
  }
}

impl<L, H, S> Services for ToolServices<L, H, S>
where
  L: StoreLender + Clone,
  H: HgStores + Clone,
  S: ShardLender + Clone,
{
  type Command = ToolCommand<L, H, S>;

  fn start(&mut self, start: &Start<'_>) -> Result<ToolCommand<L, H, S>, Unserved> {
    let version = slates_git::http::version(start.git_protocol);
    match (start.service, start.repository) {
      (Service::GitUploadPack, Some(repository)) if self.lender.holds(repository.id) => {
        Ok(ToolCommand::Upload(UploadCommand::new(
          self.lender.clone(),
          (repository.id, start.principal.clone()),
          version,
        )))
      }
      (Service::GitReceivePack, Some(repository)) => {
        Ok(ToolCommand::Receive(Box::new(ReceiveCommand::new(
          self.shard.clone(),
          start.principal.clone(),
          (repository.id, repository.name.clone()),
          version,
        ))))
      }
      (Service::Mercurial { hidden }, Some(repository)) if self.hg.holds(repository.id) => {
        Ok(ToolCommand::Mercurial(HgCommand::new(
          self.hg.clone(),
          (repository.id, start.principal.clone()),
          self.room,
          hidden,
        )))
      }
      _ => Err(Unserved),
    }
  }
}

/// A command [`ToolServices`] started: one of the tools' sessions, receive-pack's boxed, so its push's session (A-620's)
/// does not set the size every connection's command takes (clippy's `large_enum_variant` is denied).
pub(crate) enum ToolCommand<L: StoreLender, H, S> {
  /// git's upload-pack.
  Upload(UploadCommand<L>),
  /// git's receive-pack.
  Receive(Box<ReceiveCommand<S>>),
  /// Mercurial's `serve --stdio`.
  Mercurial(HgCommand<H>),
}

impl<L: StoreLender, H: HgStores, S: ShardLender> Command for ToolCommand<L, H, S> {
  fn input(&mut self, run: &[u8]) -> Result<(), Refused> {
    match self {
      ToolCommand::Upload(command) => command.input(run),
      ToolCommand::Receive(command) => command.input(run),
      ToolCommand::Mercurial(command) => command.input(run),
    }
  }

  fn input_end(&mut self) {
    match self {
      ToolCommand::Upload(command) => command.input_end(),
      ToolCommand::Receive(command) => command.input_end(),
      ToolCommand::Mercurial(command) => command.input_end(),
    }
  }

  async fn step(&mut self, gone: ClientGone<'_>) -> Result<Stepped, Refused> {
    match self {
      ToolCommand::Upload(command) => command.step(gone).await,
      ToolCommand::Receive(command) => command.step(gone).await,
      ToolCommand::Mercurial(command) => command.step(gone).await,
    }
  }

  fn output(&self) -> (&[u8], &[u8]) {
    match self {
      ToolCommand::Upload(command) => command.output(),
      ToolCommand::Receive(command) => command.output(),
      ToolCommand::Mercurial(command) => command.output(),
    }
  }

  fn sent(&mut self, stderr: usize, stdout: usize) {
    match self {
      ToolCommand::Upload(command) => command.sent(stderr, stdout),
      ToolCommand::Receive(command) => command.sent(stderr, stdout),
      ToolCommand::Mercurial(command) => command.sent(stderr, stdout),
    }
  }
}

/// The command an unserved service never starts: no value has this type.
pub(crate) enum NoCommand {}

impl Command for NoCommand {
  fn input(&mut self, _run: &[u8]) -> Result<(), Refused> {
    match *self {}
  }

  fn input_end(&mut self) {
    match *self {}
  }

  async fn step(&mut self, _gone: ClientGone<'_>) -> Result<Stepped, Refused> {
    match *self {}
  }

  fn output(&self) -> (&[u8], &[u8]) {
    match *self {}
  }

  fn sent(&mut self, _stderr: usize, _stdout: usize) {
    match *self {}
  }
}

impl Services for NotYet {
  type Command = NoCommand;

  fn start(&mut self, _start: &Start<'_>) -> Result<NoCommand, Unserved> {
    Err(Unserved)
  }
}

/// The tool a service belongs to, and how that tool's program refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
  /// git.
  Git,
  /// Mercurial.
  Mercurial,
  /// Subversion.
  Subversion,
}

impl Tool {
  /// The tool `service` belongs to.
  fn of(service: Service) -> Tool {
    match service {
      Service::GitUploadPack | Service::GitReceivePack => Tool::Git,
      Service::Mercurial { .. } => Tool::Mercurial,
      Service::Subversion => Tool::Subversion,
    }
  }

  /// The catalog's name for the tool.
  fn catalog(self) -> VcsTool {
    match self {
      Tool::Git => VcsTool::Git,
      Tool::Mercurial => VcsTool::Mercurial,
      Tool::Subversion => VcsTool::Subversion,
    }
  }

  /// `message` refused as the tool's program refuses: its prefix, the message, a line end, and its failure's status.
  fn refused(self, message: &str) -> Refused {
    let (prefix, exit_status) = match self {
      Tool::Git => ("fatal: ", GIT_DIE_STATUS),
      Tool::Mercurial => ("abort: ", HG_ABORT_STATUS),
      Tool::Subversion => ("svnserve: ", SVNSERVE_FAILURE_STATUS),
    };
    Refused {
      stderr: format!("{prefix}{message}\n").into_bytes(),
      exit_status,
    }
  }

  /// The tool's refusal of a missing repository at `path` (the module doc, step 1).
  fn not_found(self, path: &str) -> Refused {
    match self {
      Tool::Git => self.refused(&format!("'{path}' does not appear to be a git repository")),
      Tool::Mercurial | Tool::Subversion => self.refused(&format!("repository {path} not found")),
    }
  }
}

/// The rights a principal must hold on some name of a repository for `service` to start (the module doc, step 2).
fn rights(service: Service) -> &'static [Right] {
  match service {
    Service::GitUploadPack => &[Right::Read],
    Service::GitReceivePack => &[Right::Submit, Right::Rewrite, Right::Delete],
    Service::Mercurial { .. } | Service::Subversion => {
      &[Right::Read, Right::Submit, Right::Rewrite, Right::Delete]
    }
  }
}

/// The service `command` names and the path its command gives, if any.
fn asked(command: &Asked) -> (Service, Option<&[u8]>) {
  match command {
    Asked::GitUploadPack { path } => (Service::GitUploadPack, Some(path.as_slice())),
    Asked::GitReceivePack { path } => (Service::GitReceivePack, Some(path.as_slice())),
    Asked::Mercurial { repository, hidden } => (
      Service::Mercurial { hidden: *hidden },
      Some(repository.as_slice()),
    ),
    Asked::Subversion => (Service::Subversion, None),
  }
}

/// The repository name a command's `path` names: the path without one leading slash, when it is UTF-8.
fn repository_name(path: &[u8]) -> Option<&str> {
  let name = std::str::from_utf8(path).ok()?;
  Some(name.strip_prefix('/').unwrap_or(name))
}

/// The words a service the server cannot start ends with, in its tool's words (the module doc, step 3); a session whose
/// store goes away mid-way ends in them too, and so does a Subversion tunnel whose client names a repository not served
/// here (`super::svn`).
pub(crate) fn not_served(service: Service) -> Refused {
  Tool::of(service).refused(&format!("{} is not served yet", service.name()))
}

/// `bytes` as Python's `codecs.escape_encode` writes them, which 7.2's `stringutil.pprint` quotes (the module doc, step
/// 1): a quote and a backslash each after a backslash; tab, line feed and carriage return as `\t`, `\n` and `\r`; any
/// other byte below a space, and every byte from DEL up, as `\x` and two lowercase hexadecimal digits; the rest as
/// themselves.
fn python_escaped(bytes: &[u8]) -> String {
  let mut text = String::with_capacity(bytes.len());
  for &byte in bytes {
    match byte {
      b'\'' | b'\\' => {
        text.push('\\');
        text.push(char::from(byte));
      }
      b'\t' => text.push_str("\\t"),
      b'\n' => text.push_str("\\n"),
      b'\r' => text.push_str("\\r"),
      b' '..=b'~' => text.push(char::from(byte)),
      _ => text.push_str(&format!("\\x{byte:02x}")),
    }
  }
  text
}

/// 7.2's refusal of a `serve --stdio` whose repository argument `path` starts `--`, before anything is opened (the
/// module doc, step 1): the arguments it was run with listed as `stringutil.pprint` lists them, `'--hidden'` last when
/// asked. None for any other command.
fn unsafe_invocation(service: Service, path: &[u8]) -> Option<Refused> {
  let Service::Mercurial { hidden } = service else {
    return None;
  };
  if !path.starts_with(HG_UNSAFE_START) {
    return None;
  }
  let ran: [&[u8]; 4] = [b"-R", path, b"serve", b"--stdio"];
  let listed: Vec<String> = ran
    .into_iter()
    .chain(hidden.then_some(b"--hidden".as_slice()))
    .map(|argument| format!("'{}'", python_escaped(argument)))
    .collect();
  Some(Tool::Mercurial.refused(&format!(
    "potentially unsafe serve --stdio invocation: [{}]",
    listed.join(", ")
  )))
}

/// The production site: whose keys prove whom, where repositories are, and the services (the module doc).
pub(crate) struct ToolSite<K, R, S> {
  /// The keys' lookup.
  keys: K,
  /// Where repositories are found.
  repositories: R,
  /// The services.
  services: S,
}

impl<K, R, S> ToolSite<K, R, S> {
  /// A site over `keys`, `repositories` and `services`.
  pub(crate) fn new(keys: K, repositories: R, services: S) -> Self {
    Self {
      keys,
      repositories,
      services,
    }
  }
}

impl<K, R, S> ToolSite<K, R, S>
where
  R: Repositories,
{
  /// The repository at `path` for `service`, if `principal` may reach it (the module doc, steps 1 and 2).
  fn reachable(
    &self,
    service: Service,
    path: &[u8],
    principal: &Principal,
  ) -> Result<RepositoryRecord, Refused> {
    let tool = Tool::of(service);
    let shown = String::from_utf8_lossy(path);
    let Some(repository) = repository_name(path)
      .and_then(|name| self.repositories.find(name))
      .filter(|repository| repository.tool == tool.catalog())
    else {
      return Err(tool.not_found(&shown));
    };
    match vcs_policy::reaches(&repository, principal, rights(service)) {
      Ok(true) => Ok(repository),
      Ok(false) => Err(tool.refused(&format!(
        "{shown}: {} is not allowed by the repository's policy",
        service.name()
      ))),
      // A policy its tool cannot read is a damaged record (A-622's guard keeps such rules out): never judged without it.
      Err(_) => Err(tool.refused(&format!("{shown}: the repository's policy cannot be read"))),
    }
  }
}

impl<K, R, S> SshSite for ToolSite<K, R, S>
where
  K: KeyHolders<Principal = Principal> + Clone,
  R: Repositories,
  S: Services,
{
  type Keys = K;
  type Command = S::Command;

  fn keys(&mut self) -> K {
    self.keys.clone()
  }

  /// The credential record's answer for the key (the module doc): the principal it proves, with its one key to sign, or
  /// nobody, counted by why ([`KeyUnproven`]). The user name the client gave plays no part: the key alone names who
  /// connects, as `git@` is everyone's user name on a git host.
  async fn lookup(&mut self, _user: &[u8], _key: &UserKey, blob: &[u8]) -> Holding<Principal> {
    match recorded_holder(blob).await {
      Ok(principal) => Holding::Holds {
        principal,
        keys_required: KEYS_REQUIRED,
      },
      Err(unproven) => {
        crate::fleet::count_refusal(unproven.counter());
        Holding::Nobody
      }
    }
  }

  /// The shard of the repository the command names (the module doc): the name its path gives, located on the partition
  /// it routes to; a command that names none (Subversion's tunnel), or a path that is no name, is decided here.
  fn locate(&self, command: &Asked) -> Located {
    asked(command)
      .1
      .and_then(repository_name)
      .map_or(Located::Here, |name| self.repositories.locate(name))
  }

  fn dispatch(
    &mut self,
    principal: &Principal,
    command: &Asked,
    git_protocol: Option<&[u8]>,
  ) -> Dispatched<S::Command> {
    let (service, path) = asked(command);
    if let Some(refused) = path.and_then(|path| unsafe_invocation(service, path)) {
      return Dispatched::Refuse(refused);
    }
    let repository = match path.map(|path| self.reachable(service, path, principal)) {
      Some(Ok(repository)) => Some(repository),
      Some(Err(refused)) => return Dispatched::Refuse(refused),
      None => None,
    };
    let start = Start {
      service,
      repository: repository.as_ref(),
      principal,
      git_protocol,
    };
    match self.services.start(&start) {
      Ok(command) => Dispatched::Run(command),
      Err(Unserved) => Dispatched::Refuse(not_served(service)),
    }
  }
}

#[cfg(test)]
#[path = "tests/site.rs"]
mod tests;
