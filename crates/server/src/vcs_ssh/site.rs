//! What the SSH edge serves (A-651; §4.17.2 "Transports" item 2, "Access"; §4.17.3; §4.17.4's svn+ssh, piece 12): a key
//! proved to a principal of §4.13 through the site's lookup ([`SshSite::keys`], `slates_ssh`'s `KeyHolders`), and each
//! `exec` dispatched by its tool ([`SshSite::dispatch`]) to the command the edge then runs ([`Command`]). The lookup is
//! the one record of principals' credentials both edges read; until it lands, [`NoKeyRecord`] refuses every key.
//!
//! **The production site** ([`ToolSite`]) decides an `exec` in A-625 part 2's order for HTTP, each refusal written to the
//! client's standard error with its tool's own wording and exit status, as the tool's own program would refuse it:
//! 1. The repository, for git and Mercurial, whose commands name it: the one whose name is the path as the client sent
//!    it, without one leading slash, and of the command's tool; none is refused as the tool refuses a missing repository
//!    (git: `fatal: '<path>' does not appear to be a git repository`, its `die`'s 128 [C: git `builtin/upload-pack.c`,
//!    `usage.c` at `e1ec09b2`]; Mercurial: `abort: repository <path> not found`, 255 [C: Mercurial 7.2 `localrepo.py`,
//!    `scmutil.py`'s `callcatch`, `dispatch.py`'s `sys.exit(status & 255)`]). No second spelling is tried.
//! 2. The policy: a principal the repository's policy lets reach it for none of the command's rights is refused
//!    (`crate::vcs_policy::reaches`, A-597). upload-pack asks to read; receive-pack to submit, rewrite or delete;
//!    Mercurial's `serve --stdio`, which pulls and pushes in one session, any of the four, each command then judged by
//!    the session. A damaged policy is refused, never judged without it.
//! 3. The service ([`Services`]): one the server cannot start is refused as not served ([`Unserved`]), as the HTTP edge
//!    answers `503`. git's upload-pack runs (`super::git`, A-651 part 2) when the store lender holds the repository's
//!    store (A-625 part 3's [`StoreLender`]) at protocol v2 (`super::git`'s protocols), which no lender does until the
//!    lender over A-662's map of stores on their owner shards lands (A-662's owed list). Mercurial's `serve --stdio`
//!    (`slates_hg::serve::stdio::Stdio`, A-650 part 2) waits likewise for a lender of its store. git's receive-pack
//!    runs A-654 part 2's driver over the repository's store on its owner shard, the shard's own (A-662), so it needs
//!    no store lender: each of its steps is lent the shard's state, as the HTTP site's are (A-654 part 3's
//!    [`ShardLender`]; `super::receive`, A-683). Subversion's tunnel (A-656) waits for its entry point. Each becomes a
//!    [`Command`] behind [`Services`] as it can run, and the not-served answer goes with the last of them.
//!
//! Subversion's `svnserve -t` names no repository: its client names one in the greeting's URL (`svn_ra_svn` protocol), so
//! its repository and policy are the tunnel session's to decide, and the edge only starts it (svnserve's own refusals
//! begin `svnserve: ` and exit `EXIT_FAILURE` [C: `svnserve.c` at `d434dc14`]).

use slates_db::catalog::{Principal, RepositoryRecord, VcsTool};
use slates_ssh::command::Command as Asked;
use slates_ssh::userauth::{Holding, KeyHolders, UserKey};

use super::git::UploadCommand;
use super::receive::ReceiveCommand;
use crate::vcs_http::git::Repositories;
use crate::vcs_http::git::{ShardLender, StoreLender};
use crate::vcs_policy::{self, Right};

/// Format: the refusal count of a key refused because no record of principals' keys exists yet ([`NoKeyRecord`]).
const NO_KEY_RECORD: &str = "vcs_ssh.no_key_record";

/// Format: the status git's `die` exits with (`usage.c` at `e1ec09b2`: `exit(128)`), the one git-upload-pack and
/// git-receive-pack give a repository they cannot enter.
pub(crate) const GIT_DIE_STATUS: u32 = 128;
/// Format: the status `hg` exits with on an abort in Mercurial 7.2: `callcatch`'s coarse code, -1 (`scmutil.py`), through
/// `sys.exit(status & 255)` (`dispatch.py`).
pub(crate) const HG_ABORT_STATUS: u32 = 255;
/// Format: svnserve's failure, `EXIT_FAILURE` (`svnserve.c` at `d434dc14`; C's `stdlib.h`).
pub(crate) const SVNSERVE_FAILURE_STATUS: u32 = 1;

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

/// A command an `exec` started, sans-io as each tool's entry point is (Mercurial's `serve::stdio::Stdio`, A-650 part 2;
/// git's `upload::serve`, A-640 part 2): the client's input held as it arrives, up to the channel's window, until a step
/// consumes it; its standard error and output held until the channel takes them, error first.
pub(crate) trait Command {
  /// Takes one run of the client's input, held until a step consumes it; refused past what the command holds, the
  /// channel's window.
  fn input(&mut self, run: &[u8]) -> Result<(), Refused>;
  /// The client's input has ended.
  fn input_end(&mut self);
  /// Serves what it can: what it consumed of its input, and what it waits for now.
  async fn step(&mut self) -> Result<Stepped, Refused>;
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
  /// Dispatches `command` for `principal`, with the `GIT_PROTOCOL` value sent before it.
  fn dispatch(
    &mut self,
    principal: &Principal,
    command: &Asked,
    git_protocol: Option<&[u8]>,
  ) -> Dispatched<Self::Command>;
}

/// The keys' lookup until the record of principals' version-control credentials lands (main's ruling under §4.13: a
/// person enrolls them through slates' own CLI on the control channel, never through MCP or the SDK, and the metadata
/// database keeps SSH keys by fingerprint in the one record both edges' authenticators read, which g299 builds). Every key
/// is refused and counted, and nothing stands in for the record: no key file, no default key.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NoKeyRecord;

impl KeyHolders for NoKeyRecord {
  type Principal = Principal;

  fn holding(&mut self, _user: &[u8], _key: &UserKey, _blob: &[u8]) -> Holding<Principal> {
    crate::fleet::count_refusal(NO_KEY_RECORD);
    Holding::Nobody
  }
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

/// The services the server runs today: git's upload-pack from the stores `lender` lends, each request admitted `room`
/// bytes (`super::git`, A-651 part 2), and git's receive-pack through A-654 part 2's driver on the repository's owner
/// shard, whose state `shard` lends a step at a time as it lends the HTTP site's (`super::receive`, A-683), each push
/// admitted `room` bytes; Mercurial's `serve --stdio` and Subversion's tunnel not yet, as their stores (A-662) and entry
/// point (A-656) have not landed. A repository whose store the lender does not hold is not served upload-pack.
#[derive(Clone, Debug)]
pub(crate) struct ToolServices<L, S> {
  /// Where the stores are lent, for upload-pack.
  lender: L,
  /// Where the shard's state is lent, for receive-pack (in the daemon the HTTP site's `ThisShard`).
  shard: S,
  /// The bytes each request, and each push, may hold: the edge's room.
  room: u64,
}

impl<L, S> ToolServices<L, S> {
  /// The services over `lender`'s stores and the state `shard` lends, each request admitted `room` bytes.
  pub(crate) fn new(lender: L, shard: S, room: u64) -> Self {
    Self {
      lender,
      shard,
      room,
    }
  }
}

impl<L: StoreLender + Clone, S: ShardLender + Clone> Services for ToolServices<L, S> {
  type Command = ToolCommand<L, S>;

  fn start(&mut self, start: &Start<'_>) -> Result<ToolCommand<L, S>, Unserved> {
    let version = slates_git::http::version(start.git_protocol);
    match (start.service, start.repository) {
      (Service::GitUploadPack, Some(repository)) if self.lender.holds(repository.id) => {
        Ok(ToolCommand::Upload(Box::new(UploadCommand::new(
          self.lender.clone(),
          repository.id,
          version,
          self.room,
        ))))
      }
      (Service::GitReceivePack, Some(repository)) => {
        Ok(ToolCommand::Receive(Box::new(ReceiveCommand::new(
          self.shard.clone(),
          start.principal.clone(),
          (repository.id, repository.name.clone()),
          version,
          self.room,
        ))))
      }
      _ => Err(Unserved),
    }
  }
}

/// A command [`ToolServices`] runs: git's upload-pack or its receive-pack, each boxed, so the command a connection holds
/// is a pointer whichever it is.
pub(crate) enum ToolCommand<L: StoreLender, S> {
  /// git's upload-pack (`super::git`).
  Upload(Box<UploadCommand<L>>),
  /// git's receive-pack (`super::receive`).
  Receive(Box<ReceiveCommand<S>>),
}

impl<L: StoreLender, S: ShardLender> Command for ToolCommand<L, S> {
  fn input(&mut self, run: &[u8]) -> Result<(), Refused> {
    match self {
      ToolCommand::Upload(command) => command.input(run),
      ToolCommand::Receive(command) => command.input(run),
    }
  }

  fn input_end(&mut self) {
    match self {
      ToolCommand::Upload(command) => command.input_end(),
      ToolCommand::Receive(command) => command.input_end(),
    }
  }

  async fn step(&mut self) -> Result<Stepped, Refused> {
    match self {
      ToolCommand::Upload(command) => command.step().await,
      ToolCommand::Receive(command) => command.step().await,
    }
  }

  fn output(&self) -> (&[u8], &[u8]) {
    match self {
      ToolCommand::Upload(command) => command.output(),
      ToolCommand::Receive(command) => command.output(),
    }
  }

  fn sent(&mut self, stderr: usize, stdout: usize) {
    match self {
      ToolCommand::Upload(command) => command.sent(stderr, stdout),
      ToolCommand::Receive(command) => command.sent(stderr, stdout),
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

  async fn step(&mut self) -> Result<Stepped, Refused> {
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

  fn dispatch(
    &mut self,
    principal: &Principal,
    command: &Asked,
    git_protocol: Option<&[u8]>,
  ) -> Dispatched<S::Command> {
    let (service, path) = asked(command);
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
      Err(Unserved) => Dispatched::Refuse(
        Tool::of(service).refused(&format!("{} is not served yet", service.name())),
      ),
    }
  }
}

#[cfg(test)]
#[path = "tests/site.rs"]
mod tests;
