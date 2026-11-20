//! Subversion's `svnserve -t` behind the SSH edge (A-688; §4.17.4 "The wire" (svn+ssh) and piece 12, A-656's
//! connection): slates-svn's one ra_svn connection ([`Connection`], the one `svn://` serves too), driven over an SSH
//! session's standard input and output as `svnserve -t` serves its tunnel, the principal the one the client's key
//! proved.
//!
//! **The session.** As `svnserve -t` does, the tunnel speaks first: the greeting is written when the command starts.
//! The client's answer names a URL, and the tunnel finds the repository it names as `svnserve`'s `find_repos` does
//! ([`found`]): the path beneath the URL's scheme and host (`skip_scheme_part`: `svn`, an optional `+` and a tunnel's
//! name, `://`, then the host up to the first `/`), each segment percent-decoded, a `..` segment refused "Couldn't
//! determine repository path", then the longest run of leading segments that names a Subversion repository here, as
//! `svn_repos_find_root_path` walks up from the whole path. The repository's root URL is the client's URL cut after
//! that run. A URL of another form is refused "Non-svn URL passed to svn server", one no repository holds "No
//! repository found in '…'", and one whose repository's policy lets the principal reach it for none of its rights
//! (A-597's `vcs_policy::reaches`, as the site judges Mercurial's `serve --stdio`) "No access allowed to this
//! repository". A repository whose store is not held here ([`SvnStores::holds`]) is not served, in the site's words,
//! with nothing written to the client.
//!
//! **Who.** The tunnel's user, which `EXTERNAL` takes (`handshake::Repository::tunnel_user`), is the principal the
//! client's key proved (§4.13; §4.17.4: "the principal the one the client's key proved"), named as slates'
//! version-control answers show a principal until §4.13 names principals for people: its index key in hexadecimal
//! (`crate::lfs::owner_name`, the name an LFS lock's owner is shown by). The SSH login the client sent proves nothing
//! and names no one here: a client could send any login, so taking it would let any key holder name any user.
//!
//! **The sources** ([`SvnStores`]). What the handshake tells the client (the UUID, the realm, mergeinfo, who may read)
//! and, from the session's establishment on, the repository's sources (its index, green, lock table, policy and the
//! owner's instant) are lent afresh for each step, as git's site lends its stores (A-640 part 2: "so a shard's state is
//! never held across a wait"). A store that goes away mid-session ends it in the not-served words.
//!
//! **A commit.** The client's `commit` begins the drive (`Connection::commit`) with its transaction named B, a `-` and
//! this connection's count of commits begun in base 36, B the youngest revision: §4.17.4 item 1 names it by the work's
//! id, and the tunnel opens no work until it calls the owner's commit step. A hook the drive waits for (A-661) passes
//! where the repository's policy configures none at its point, as `svnserve` answers a point with no hook
//! (`crate::svn_hooks`); where one is configured the commit is refused there with the not-served words, since running a
//! hook needs the transaction's view, owed with the commit step, and no hook is ever skipped. An accepted work is
//! refused `Uncommitted::NotServed`, "This server does not make commits yet" (200007), until the tunnel hands it to the
//! owner's commit step (A-655 part 2b's `repository::svn_commit::commit`, which runs on the owner shard's state and
//! which neither ra_svn transport calls yet).
//!
//! **How the tunnel ends**, as stock `svnserve -t` 1.15.0 ends: the error that ended the connection on standard error,
//! `svnserve: E` and its number in six digits, `: ` and its message, and `EXIT_FAILURE` (1); a clean end, 0 with
//! nothing. Recorded on the Mac Studio, 2026-11-19T22:27Z and 22:34Z, scripted clients' bytes piped to `svnserve -t
//! --tunnel-user=alice -r ROOT` (the tests' oracle, `tests/svn.rs`): a client that leaves between commands, or inside a
//! command's item, 0; a malformed item, E210004 "Malformed network data"; a client that leaves during the handshake,
//! inside a report or inside a commit's edit, E210002 "Network connection closed unexpectedly"; a URL no repository
//! holds, E210005, one of another scheme, E125002, one climbing out with `..`, E125001, and a repository closed to the
//! user, E170001, each written to the client too; protocol version 3 and a client without `edit-pipeline`, E210006 in
//! the handshake's words. A client that leaves while its refused edit's commands are being dropped ends E210002 here
//! where `svnserve` exits 0 (recorded: `open-dir` of a missing path, then the client gone), since the connection's
//! [`Waiting`] does not tell the drop from a live edit (GAPS); one that ends the drop with `abort-edit` first ends 0 as
//! there. Where `svnserve` aborts (`serve::Refused::Aborted`) and on the caller's own errors, the tunnel ends 1 with
//! nothing more, counted: an abort is a signal, which an exit status cannot carry.
//!
//! **Bounds** ([`TunnelBounds`]) are the caller's, each derived from its shard's budget when the daemon starts the edge
//! (owed with it, as `ToolServices`' room is): one item's bytes, a report's room and the nodes a drive holds open
//! ([`Limits`]), what one step writes and its drive's steps ([`Budget`]), and a commit's room. The client's input held
//! is at most the channel's window, which the session refuses to exceed, and each step gives back what it consumed.
//!
//! **Owed** (GAPS, the §4.17 entry): the lender over A-662's map (the records A-655 part 2b keeps in its Subversion
//! arm: the index, revision properties, contents, lock table and UUID; with a directory's entries from the green and
//! the repository's `svnserve.conf` facts; the HTTP edge's DAV site waits for the same lender behind its own
//! `SvnStores`); the accepted work handed to the owner's commit step, and the hooks' view; CRAM-MD5 through the tunnel,
//! whose keys the record of credentials does not hold yet (a lender that lists it gets every challenge failed, as
//! `svnserve` fails one it cannot make); a URL canonicalized as `svn_uri_canonicalize` would (stock clients send
//! canonical URLs); the drop's ending above.

use slates_db::catalog::{Principal, RepositoryId, RepositoryRecord, VcsTool};
use slates_svn::commit::drive::{Due, Green};
use slates_svn::handshake::{self, Access, Repository, Unserved as Refusal};
use slates_svn::ra_svn::ItemError;
use slates_svn::reads;
use slates_svn::report::{self, exchange::Budget};
use slates_svn::serve::{
  self, CommitStart, Connection, Limits, Sources, Turn, Uncommitted, Waiting,
};

use super::site::{
  Command, Next, Refused, SVNSERVE_FAILURE_STATUS, Service, Services, Start, Stepped, Unserved,
  not_served,
};
use crate::vcs_hooks::HookPoint;
use crate::vcs_http::git::Repositories;
use crate::vcs_policy::{self, Right};

/// Format: the refusal count of a command the connection answered "Unknown editor command" because slates does not
/// serve it yet (`Turn::Unserved`).
const UNSERVED_COMMAND: &str = "vcs_ssh.svn_unserved_command";
/// Format: the refusal count of a session whose repository's store is not held here, or went away.
const STORE_NOT_HELD: &str = "vcs_ssh.svn_store_not_held";
/// Format: the refusal count of a session refused "No access allowed to this repository" by the repository's policy.
const NO_ACCESS: &str = "vcs_ssh.svn_no_access";
/// Format: the refusal count of a tunnel ended with no words: where `svnserve` aborts, or on the tunnel's own error.
const UNWORDED_END: &str = "vcs_ssh.svn_unworded_end";

/// Format: the scheme every ra_svn URL starts with (`serve.c` `skip_scheme_part`).
const SVN_SCHEME: &[u8] = b"svn";
/// Format: what follows a URL's scheme (`skip_scheme_part`).
const SCHEME_END: &[u8] = b"://";
/// Format: the segment that climbs out of the server's root (`serve.c` `repos_path_valid`).
const PARENT_SEGMENT: &[u8] = b"..";
/// Format: the digits of base 36, lower-case, which a transaction's name keeps to (`svn_fs.h`'s letters and digits).
const BASE36_DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
/// Format: base 36's radix.
const BASE36_RADIX: u64 = 36;
/// Format: a percent-encoded byte's hexadecimal radix (RFC 3986 §2.1).
const HEX_RADIX: u32 = 16;

/// Format: `SVN_ERR_RA_SVN_CONNECTION_CLOSED`, the ra_svn category's start (210,000) plus 2 (`svn_error_codes.h`).
const CONNECTION_CLOSED: u32 = 210_002;
/// Format: `SVN_ERR_RA_SVN_MALFORMED_DATA`, the ra_svn category's start plus 4.
const MALFORMED_DATA: u32 = 210_004;
/// Format: `SVN_ERR_RA_SVN_REQUEST_SIZE`, the ra_svn category's start plus 9.
const REQUEST_SIZE: u32 = 210_009;
/// Format: `SVN_ERR_UNSUPPORTED_FEATURE`, the miscellaneous category's start (200,000) plus 7.
const UNSUPPORTED_FEATURE: u32 = 200_007;

/// The words of a client that left where the connection still read (recorded, the module doc).
const CLOSED_WORDS: &str = "Network connection closed unexpectedly";
/// The words of bytes that are no item (`marshal.c`; recorded).
const MALFORMED_WORDS: &str = "Malformed network data";
/// The words of an item past its bound: `SVN_ERR_RA_SVN_REQUEST_SIZE`'s own (slates-svn's `report::Refused`).
const TOO_LONG_WORDS: &str = "Client request too long";
/// The words a commit is refused with until the tunnel calls the owner's commit step (slates-svn's
/// `Uncommitted::NotServed`).
const NOT_COMMITTED_WORDS: &str = "This server does not make commits yet";

/// The rights some name of a repository must grant a principal for its tunnel to open it: any of the four, each command
/// then judged by the session, as the site judges Mercurial's `serve --stdio`.
const TUNNEL_RIGHTS: [Right; 4] = [Right::Read, Right::Submit, Right::Rewrite, Right::Delete];

/// What a Subversion repository's sessions are opened with beside its sources: what `svnserve`'s `find_repos` and its
/// `svnserve.conf` give `repos-info` and the authentication request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Opening<'a> {
  /// The repository's UUID.
  pub(crate) uuid: &'a [u8],
  /// The authentication realm (`svnserve`'s default is the UUID).
  pub(crate) realm: &'a [u8],
  /// Whether the repository keeps mergeinfo, which `repos-info` then announces.
  pub(crate) mergeinfo: bool,
  /// Whether anyone may read without authenticating (`ANONYMOUS`).
  pub(crate) anonymous: bool,
  /// Whether authenticated users may read, so the tunnel's user is offered (`EXTERNAL`).
  pub(crate) authenticated: bool,
  /// Whether authenticated users may read and the repository holds their keys (`CRAM-MD5`).
  pub(crate) cram_md5: bool,
}

/// Where Subversion repositories' stores are lent on this shard (A-662: each repository's tool store, kept on its owner
/// shard with its record), afresh for each step, as git's site's `StoreLender` and Mercurial's `HgStores` lend theirs.
pub(crate) trait SvnStores {
  /// The repository's green, which the connection's reads and a commit's drive read.
  type Green: reads::Repository + Green;
  /// Whether the store of repository `id` is held here.
  fn holds(&self, id: RepositoryId) -> bool;
  /// Runs `step` over what repository `id`'s sessions are opened with; none when its store is not held here.
  fn opening<T>(&mut self, id: RepositoryId, step: impl FnOnce(&Opening<'_>) -> T) -> Option<T>;
  /// Runs `step` over repository `id`'s sources at the owner's instant; none when its store is not held here.
  fn lend<T>(
    &mut self,
    id: RepositoryId,
    step: impl FnOnce(&Sources<'_, Self::Green>) -> T,
  ) -> Option<T>;
}

/// A tunnel's bounds, each its caller's (the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TunnelBounds {
  /// The connection's: one item's bytes, a report's room, the nodes a drive holds open.
  pub(crate) limits: Limits,
  /// What one step may write, and the steps a drive takes for it.
  pub(crate) budget: Budget,
  /// The most bytes a commit's new texts and property lists may hold.
  pub(crate) commit_room: usize,
}

/// The services with Subversion's tunnel served beside `other`'s: an `exec` of `svnserve -t` runs a tunnel over the
/// repositories `repositories` finds and the stores `stores` lends; every other service is `other`'s to start. The
/// tunnel needs the lookup itself, since its client names its repository inside the session, not in the `exec`.
pub(crate) struct SvnServices<S, R, V> {
  /// The other services.
  other: S,
  /// Where repositories are found by name.
  repositories: R,
  /// Where their stores are lent.
  stores: V,
  /// Each tunnel's bounds.
  bounds: TunnelBounds,
}

impl<S, R, V> SvnServices<S, R, V> {
  /// Subversion's tunnel over `repositories` and `stores`, each within `bounds`, beside `other`'s services.
  pub(crate) fn new(other: S, repositories: R, stores: V, bounds: TunnelBounds) -> Self {
    Self {
      other,
      repositories,
      stores,
      bounds,
    }
  }
}

/// The command a site with the tunnel runs: another service's, or the tunnel.
pub(crate) enum EitherCommand<C, T> {
  /// Another service's command.
  Other(C),
  /// Subversion's tunnel.
  Tunnel(T),
}

impl<C: Command, T: Command> Command for EitherCommand<C, T> {
  fn input(&mut self, run: &[u8]) -> Result<(), Refused> {
    match self {
      Self::Other(command) => command.input(run),
      Self::Tunnel(command) => command.input(run),
    }
  }

  fn input_end(&mut self) {
    match self {
      Self::Other(command) => command.input_end(),
      Self::Tunnel(command) => command.input_end(),
    }
  }

  async fn step(&mut self) -> Result<Stepped, Refused> {
    match self {
      Self::Other(command) => command.step().await,
      Self::Tunnel(command) => command.step().await,
    }
  }

  fn output(&self) -> (&[u8], &[u8]) {
    match self {
      Self::Other(command) => command.output(),
      Self::Tunnel(command) => command.output(),
    }
  }

  fn sent(&mut self, stderr: usize, stdout: usize) {
    match self {
      Self::Other(command) => command.sent(stderr, stdout),
      Self::Tunnel(command) => command.sent(stderr, stdout),
    }
  }
}

impl<S, R, V> Services for SvnServices<S, R, V>
where
  S: Services,
  R: Repositories + Clone,
  V: SvnStores + Clone,
{
  type Command = EitherCommand<S::Command, SvnCommand<R, V>>;

  fn start(&mut self, start: &Start<'_>) -> Result<Self::Command, Unserved> {
    if start.service != Service::Subversion {
      return self.other.start(start).map(EitherCommand::Other);
    }
    SvnCommand::new(
      self.repositories.clone(),
      self.stores.clone(),
      start.principal,
      self.bounds,
    )
    .map(EitherCommand::Tunnel)
    .ok_or(Unserved)
  }
}

/// One `svnserve -t` session over SSH (the module doc).
pub(crate) struct SvnCommand<R, V> {
  /// Where repositories are found by name.
  repositories: R,
  /// Where their stores are lent.
  stores: V,
  /// The principal the client's key proved, which the repository's policy judges.
  principal: Principal,
  /// Its name, the tunnel's user.
  user: Vec<u8>,
  /// The connection.
  connection: Connection,
  /// The repository the session opened, once its URL was found and answered.
  opened: Option<RepositoryRecord>,
  /// The client's input held, not yet consumed.
  input: Vec<u8>,
  /// Whether the client's input has ended.
  input_ended: bool,
  /// What the connection wrote that the channel has not taken.
  out: Vec<u8>,
  /// What the tunnel wrote on standard error that the channel has not taken.
  err: Vec<u8>,
  /// The status it exits with once its output is sent; none while it serves.
  exit: Option<u32>,
  /// Its bounds.
  bounds: TunnelBounds,
  /// The commits the connection began, which name their transactions.
  commits: u64,
}

/// What one call of the connection made of the client's bytes, owned, so the input can be consumed after it.
enum Received {
  /// The input ends inside the client's next item.
  NeedMore,
  /// These bytes were taken and answered, or taken with nothing for the tunnel to do.
  Took(usize),
  /// Output was written and nothing taken.
  Wrote,
  /// The client answered the greeting with this URL.
  Opened {
    /// The URL.
    url: Vec<u8>,
    /// The bytes taken.
    consumed: usize,
  },
  /// The client chose CRAM-MD5.
  Challenge(usize),
  /// The client's CRAM-MD5 response named a user.
  Key(usize),
  /// The client asked for a commit.
  CommitAsked(usize),
  /// The commit's drive waits for a hook.
  Hook {
    /// The hook.
    due: Due,
    /// The bytes taken.
    consumed: usize,
  },
  /// The commit's drive accepted its work.
  Accepted(usize),
  /// The connection refused the client's bytes and ended.
  Ended(serve::Refused),
  /// The repository's store is not held here any longer.
  NotHeld,
}

impl Received {
  /// `turn`, owned.
  fn of(turn: Turn<'_>) -> Received {
    match turn {
      Turn::NeedMore { .. } => Received::NeedMore,
      Turn::Took { consumed } | Turn::Established { consumed, .. } => Received::Took(consumed),
      Turn::Unserved { consumed, .. } => {
        crate::fleet::count_refusal(UNSERVED_COMMAND);
        Received::Took(consumed)
      }
      Turn::Wrote => Received::Wrote,
      Turn::Opened { open, consumed } => Received::Opened {
        url: open.url.to_vec(),
        consumed,
      },
      Turn::ChallengeNeeded { consumed } => Received::Challenge(consumed),
      Turn::KeyNeeded { consumed, .. } => Received::Key(consumed),
      Turn::CommitAsked { consumed } => Received::CommitAsked(consumed),
      Turn::Hook { due, consumed } => Received::Hook { due, consumed },
      Turn::Accepted { consumed, .. } => Received::Accepted(consumed),
    }
  }
}

impl<R: Repositories, V: SvnStores> SvnCommand<R, V> {
  /// A session for `principal` over `repositories` and `stores` within `bounds`, its greeting written; none when the
  /// greeting cannot be written, which its grammar's words never fail.
  pub(crate) fn new(
    repositories: R,
    stores: V,
    principal: &Principal,
    bounds: TunnelBounds,
  ) -> Option<Self> {
    let mut out = Vec::new();
    let connection = Connection::new(bounds.limits, &mut out).ok()?;
    Some(Self {
      repositories,
      stores,
      principal: principal.clone(),
      user: crate::lfs::owner_name(principal).into_bytes(),
      connection,
      opened: None,
      input: Vec::new(),
      input_ended: false,
      out,
      err: Vec::new(),
      exit: None,
      bounds,
      commits: 0,
    })
  }

  /// One step (the module doc): the connection's call over the input held, the tunnel's answer to what it asks, and the
  /// bytes it took given back.
  fn stepped(&mut self) -> Stepped {
    if let Some(status) = self.exit {
      return exited(status);
    }
    let consumed = match self.received() {
      Received::NeedMore => return self.wait_or_end(),
      Received::Wrote => 0,
      Received::Took(consumed) => consumed,
      Received::Opened { url, consumed } => {
        self.open(&url);
        consumed
      }
      Received::Challenge(consumed) => {
        let failed = self.connection.challenge_failed(&mut self.out);
        self.end_if_refused(failed);
        consumed
      }
      Received::Key(consumed) => {
        let verdict = self.connection.key(None, &mut self.out).map(|_| ());
        self.end_if_refused(verdict);
        consumed
      }
      Received::CommitAsked(consumed) => {
        self.begin_commit();
        consumed
      }
      Received::Hook { due, consumed } => {
        self.answer_hook(&due);
        consumed
      }
      Received::Accepted(consumed) => {
        self.refuse_work();
        consumed
      }
      Received::Ended(refused) => {
        self.end(refused);
        0
      }
      Received::NotHeld => {
        self.not_held();
        0
      }
    };
    self.consumed(consumed)
  }

  /// The connection's call over the input held: with the repository's sources once the session is established.
  fn received(&mut self) -> Received {
    let budget = self.bounds.budget;
    let Self {
      connection,
      stores,
      opened,
      input,
      out,
      ..
    } = self;
    if connection.session().is_none() {
      let sources: Option<&Sources<'_, V::Green>> = None;
      return connection
        .receive(sources, input, out, budget)
        .map_or_else(Received::Ended, Received::of);
    }
    let Some(id) = opened.as_ref().map(|record| record.id) else {
      return Received::NotHeld;
    };
    stores
      .lend(id, |sources| {
        connection
          .receive(Some(sources), input, out, budget)
          .map_or_else(Received::Ended, Received::of)
      })
      .unwrap_or(Received::NotHeld)
  }

  /// `consumed` bytes of the input done with: the step that took them, the tunnel's end where one was reached.
  fn consumed(&mut self, consumed: usize) -> Stepped {
    let taken = consumed.min(self.input.len());
    self.input.drain(..taken);
    Stepped {
      consumed: taken,
      next: self.exit.map_or(Next::More, Next::Exited),
    }
  }

  /// The input ends inside the client's next item: wait for more, or, once it has ended, end as `svnserve` ends there.
  fn wait_or_end(&mut self) -> Stepped {
    if !self.input_ended {
      return Stepped {
        consumed: 0,
        next: Next::Input,
      };
    }
    match self.connection.waiting() {
      Waiting::Command | Waiting::Ended => self.exit = Some(0),
      Waiting::Handshake
      | Waiting::Report
      | Waiting::Writing
      | Waiting::EditAnswer
      | Waiting::Edit => self.ends_with(CONNECTION_CLOSED, CLOSED_WORDS.as_bytes()),
    }
    self.consumed(0)
  }

  /// Answers the client's `url` (the module doc's session): the repository it names opened, or the URL refused.
  fn open(&mut self, url: &[u8]) {
    let (record, root) = match found(&self.repositories, url) {
      Found::Repository { record, root } => (record, root),
      Found::BadUrl => return self.refuse_url(Refusal::BadUrl { url }),
      Found::BadPath => return self.refuse_url(Refusal::BadPath),
      Found::NoRepository => return self.refuse_url(Refusal::NoRepository { url }),
    };
    if !matches!(
      vcs_policy::reaches(&record, &self.principal, &TUNNEL_RIGHTS),
      Ok(true)
    ) {
      crate::fleet::count_refusal(NO_ACCESS);
      return self.refuse_url(Refusal::NoAccess);
    }
    if !self.stores.holds(record.id) {
      return self.not_held();
    }
    let root_url = url.get(..root).unwrap_or(url);
    let Self {
      connection,
      stores,
      user,
      out,
      ..
    } = self;
    let opened = stores.opening(record.id, |opening| {
      let repository = Repository {
        uuid: opening.uuid,
        root_url,
        realm: opening.realm,
        mergeinfo: opening.mergeinfo,
        anonymous: opening.anonymous,
        tunnel_user: opening.authenticated.then_some(user.as_slice()),
        cram_md5: opening.cram_md5,
      };
      connection.open(&repository, out)
    });
    match opened {
      None => self.not_held(),
      Some(Ok(Access::Asked)) => self.opened = Some(record),
      Some(Ok(Access::Denied)) => {
        let (code, message) = Refusal::NoAccess.failure();
        self.ends_with(code, &message);
      }
      Some(Err(refused)) => self.end(refused),
    }
  }

  /// Refuses the client's URL with `refusal`, written as `svnserve` writes it; the tunnel ends with it, as `svnserve`'s
  /// does (recorded: "No repository found in '…'", E210005).
  fn refuse_url(&mut self, refusal: Refusal<'_>) {
    let (code, message) = refusal.failure();
    match self.connection.refuse(refusal, &mut self.out) {
      Ok(()) => self.ends_with(code, &message),
      Err(refused) => self.end(refused),
    }
  }

  /// Begins the commit the client asked for, its transaction named as the module doc says, over the lent sources.
  fn begin_commit(&mut self) {
    self.commits = self.commits.saturating_add(1);
    let commits = self.commits;
    let room = self.bounds.commit_room;
    let Some(id) = self.opened.as_ref().map(|record| record.id) else {
      return self.not_held();
    };
    let Self {
      connection,
      stores,
      out,
      ..
    } = self;
    let begun = stores.lend(id, |sources| {
      let start = CommitStart {
        transaction: transaction_name(sources.index.youngest(), commits),
        now: sources.now,
        room,
      };
      connection.commit(start, sources, out)
    });
    match begun {
      None => self.not_held(),
      Some(result) => self.end_if_refused(result),
    }
  }

  /// Answers the hook the drive waits for (the module doc's commit): passed where none is configured at its point,
  /// refused in the not-served words where one is.
  fn answer_hook(&mut self, due: &Due) {
    let point = match due {
      Due::StartCommit { .. } => HookPoint::SvnStartCommit,
      Due::PreCommit { .. } => HookPoint::SvnPreCommit,
    };
    let Some(record) = self.opened.as_ref() else {
      return self.not_held();
    };
    let id = record.id;
    // The configured hook, found as `svn_hooks::configured` finds it: that module is Unix's alone, with its driver.
    let configured = record
      .policy
      .hooks
      .iter()
      .any(|hook| HookPoint::of(hook.point) == point);
    let answer = if configured {
      Err(vec![(UNSUPPORTED_FEATURE, NOT_COMMITTED_WORDS.to_owned())])
    } else {
      Ok(())
    };
    let Self {
      connection,
      stores,
      out,
      ..
    } = self;
    let answered = stores.lend(id, |sources| connection.answer_hook(answer, sources, out));
    match answered {
      None => self.not_held(),
      Some(Ok(Turn::Accepted { .. })) => self.refuse_work(),
      Some(Ok(_)) => {}
      Some(Err(refused)) => self.end(refused),
    }
  }

  /// Refuses the accepted work: the daemon makes no commits yet (the module doc).
  fn refuse_work(&mut self) {
    let refused = self
      .connection
      .not_committed(&Uncommitted::NotServed, &mut self.out);
    self.end_if_refused(refused);
  }

  /// Ends the tunnel where `result` is the connection's refusal.
  fn end_if_refused(&mut self, result: Result<(), serve::Refused>) {
    if let Err(refused) = result {
      self.end(refused);
    }
  }

  /// Ends the tunnel on the connection's `refused`, as `svnserve` ends on that error (the module doc).
  fn end(&mut self, refused: serve::Refused) {
    match ended_words(refused) {
      Some((code, message)) => self.ends_with(code, message.as_bytes()),
      None => {
        crate::fleet::count_refusal(UNWORDED_END);
        self.exit = Some(SVNSERVE_FAILURE_STATUS);
      }
    }
  }

  /// Ends the tunnel because the repository's store is not held here: the site's not-served words, nothing more
  /// written.
  fn not_held(&mut self) {
    crate::fleet::count_refusal(STORE_NOT_HELD);
    let refused = not_served(Service::Subversion);
    self.err.extend_from_slice(&refused.stderr);
    self.exit = Some(refused.exit_status);
  }

  /// Ends the tunnel with `svnserve`'s words for error `code` with `message` on standard error, and its failure's
  /// status.
  fn ends_with(&mut self, code: u32, message: &[u8]) {
    self
      .err
      .extend_from_slice(format!("svnserve: E{code:06}: ").as_bytes());
    self.err.extend_from_slice(message);
    self.err.push(b'\n');
    self.exit = Some(SVNSERVE_FAILURE_STATUS);
  }
}

impl<R: Repositories, V: SvnStores> Command for SvnCommand<R, V> {
  fn input(&mut self, run: &[u8]) -> Result<(), Refused> {
    self.input.extend_from_slice(run);
    Ok(())
  }

  fn input_end(&mut self) {
    self.input_ended = true;
  }

  async fn step(&mut self) -> Result<Stepped, Refused> {
    Ok(self.stepped())
  }

  fn output(&self) -> (&[u8], &[u8]) {
    (&self.err, &self.out)
  }

  fn sent(&mut self, stderr: usize, stdout: usize) {
    self.err.drain(..stderr.min(self.err.len()));
    self.out.drain(..stdout.min(self.out.len()));
  }
}

/// A step that ended the tunnel with `status`.
fn exited(status: u32) -> Stepped {
  Stepped {
    consumed: 0,
    next: Next::Exited(status),
  }
}

/// Where a client's URL leads (the module doc's session).
enum Found {
  /// To this repository, whose root URL is the URL's first `root` bytes.
  Repository {
    /// The repository.
    record: RepositoryRecord,
    /// Where its root URL ends in the client's URL.
    root: usize,
  },
  /// The URL is no ra_svn URL.
  BadUrl,
  /// The URL's path climbs out with `..`.
  BadPath,
  /// No Subversion repository here holds the URL's path.
  NoRepository,
}

/// Where `url` leads among `repositories` (the module doc's session).
fn found<R: Repositories>(repositories: &R, url: &[u8]) -> Found {
  let Some(path_at) = path_start(url) else {
    return Found::BadUrl;
  };
  let Some(segments) = segments(url, path_at) else {
    return Found::BadPath;
  };
  (1..=segments.len())
    .rev()
    .find_map(|count| {
      let names = segments.get(..count)?;
      let name = names
        .iter()
        .map(|(segment, _)| std::str::from_utf8(segment).ok())
        .collect::<Option<Vec<_>>>()?
        .join("/");
      let record = repositories
        .find(&name)
        .filter(|record| record.tool == VcsTool::Subversion)?;
      let root = names.last().map(|(_, end)| *end)?;
      Some(Found::Repository { record, root })
    })
    .unwrap_or(Found::NoRepository)
}

/// Where the path of `url` starts, as `svnserve`'s `skip_scheme_part` finds it (the module doc); none for a URL of
/// another form.
fn path_start(url: &[u8]) -> Option<usize> {
  let mut at = SVN_SCHEME.len();
  if url.get(..at)? != SVN_SCHEME {
    return None;
  }
  if url.get(at) == Some(&b'+') {
    let rest = url.get(at..)?;
    at = at.checked_add(
      rest
        .iter()
        .position(|byte| *byte == b':')
        .unwrap_or(rest.len()),
    )?;
  }
  if !url.get(at..)?.starts_with(SCHEME_END) {
    return None;
  }
  at = at.checked_add(SCHEME_END.len())?;
  let host = url.get(at..)?;
  at.checked_add(
    host
      .iter()
      .position(|byte| *byte == b'/')
      .unwrap_or(host.len()),
  )
}

/// The path's segments from `path_at` in `url`, each percent-decoded with where it ends in `url`, empty ones passed
/// over; none for a `..` segment (`repos_path_valid`).
fn segments(url: &[u8], path_at: usize) -> Option<Vec<(Vec<u8>, usize)>> {
  let path = url.get(path_at..)?;
  let mut found = Vec::new();
  let mut start = 0usize;
  for (index, raw) in path.split(|byte| *byte == b'/').enumerate() {
    if index > 0 {
      start = start.checked_add(1)?;
    }
    let end = start.checked_add(raw.len())?;
    let decoded = percent_decoded(raw);
    if decoded == PARENT_SEGMENT {
      return None;
    }
    if !decoded.is_empty() {
      found.push((decoded, path_at.checked_add(end)?));
    }
    start = end;
  }
  Some(found)
}

/// `raw` with every `%` and two hexadecimal digits made the byte they name, as `svn_path_uri_decode` decodes a path
/// (`apr_isxdigit` on both), and every other byte kept.
fn percent_decoded(raw: &[u8]) -> Vec<u8> {
  let mut decoded = Vec::with_capacity(raw.len());
  let mut rest = raw;
  while let Some((&byte, after)) = rest.split_first() {
    let named = match after {
      [high, low, ..] if byte == b'%' => hex_digit(*high)
        .zip(hex_digit(*low))
        .and_then(|(high, low)| u8::try_from(high.checked_mul(HEX_RADIX)?.checked_add(low)?).ok()),
      _ => None,
    };
    match named {
      Some(value) => {
        decoded.push(value);
        rest = after.get(2..).unwrap_or_default();
      }
      None => {
        decoded.push(byte);
        rest = after;
      }
    }
  }
  decoded
}

/// The value of the hexadecimal digit `byte`, either case; none for any other byte.
fn hex_digit(byte: u8) -> Option<u32> {
  char::from(byte).to_digit(HEX_RADIX)
}

/// The name of the transaction a connection's `count`th commit begins on `base` (the module doc's commit).
fn transaction_name(base: u64, count: u64) -> String {
  let mut digits = Vec::new();
  let mut left = count;
  loop {
    let digit = left
      .checked_rem(BASE36_RADIX)
      .and_then(|digit| usize::try_from(digit).ok())
      .and_then(|digit| BASE36_DIGITS.get(digit))
      .copied()
      .unwrap_or(b'0');
    digits.push(digit);
    left = left.checked_div(BASE36_RADIX).unwrap_or(0);
    if left == 0 {
      break;
    }
  }
  digits.reverse();
  format!("{base}-{}", String::from_utf8_lossy(&digits))
}

/// The error `svnserve`'s process ends with on the connection's `refused` (the module doc's endings): its number and
/// message; none where `svnserve` aborts and for the tunnel's own errors.
fn ended_words(refused: serve::Refused) -> Option<(u32, String)> {
  match refused {
    serve::Refused::Handshake(refused) => handshake_words(refused),
    serve::Refused::Report(refused) => report_words(refused),
    serve::Refused::Item(error) => Some(item_words(error)),
    serve::Refused::Malformed => Some((MALFORMED_DATA, MALFORMED_WORDS.to_owned())),
    serve::Refused::Aborted
    | serve::Refused::OutOfTurn
    | serve::Refused::NoSources
    | serve::Refused::RootUrl => None,
  }
}

/// The error of the handshake's `refused`, in `svnserve`'s words (`serve.c`; the versions' recorded).
fn handshake_words(refused: handshake::Refused) -> Option<(u32, String)> {
  let code = refused.subversion_code()?;
  let message = match refused {
    handshake::Refused::Item(error) => return Some(item_words(error)),
    handshake::Refused::NotAnswer | handshake::Refused::NotAuthResponse => {
      MALFORMED_WORDS.to_owned()
    }
    handshake::Refused::CapabilityNotWord => "Capability entry is not a word".to_owned(),
    handshake::Refused::UnsupportedVersion(version) => {
      format!("Unsupported ra_svn protocol version {version} (supported versions: [2])")
    }
    handshake::Refused::NoEditPipeline => "Missing edit-pipeline capability".to_owned(),
    handshake::Refused::OutOfTurn => return None,
  };
  Some((code, message))
}

/// The error of the report's `refused`, in `svnserve`'s words.
fn report_words(refused: report::Refused) -> Option<(u32, String)> {
  match refused {
    report::Refused::Item(error) => Some(item_words(error)),
    report::Refused::Malformed => Some((MALFORMED_DATA, MALFORMED_WORDS.to_owned())),
    report::Refused::ReportTooLarge => Some((REQUEST_SIZE, TOO_LONG_WORDS.to_owned())),
    report::Refused::OutOfTurn | report::Refused::Editor(_) => None,
  }
}

/// The error of the item codec's `error`, in `marshal.c`'s words.
fn item_words(error: ItemError) -> (u32, String) {
  let message = match error {
    ItemError::Malformed { .. } | ItemError::InvalidWord => MALFORMED_WORDS,
    ItemError::WordTooLong { .. } => "Word is too long",
    ItemError::NumberTooLarge { .. } => "Number is larger than maximum",
    ItemError::NestedTooDeep { .. } => "Items are nested too deeply",
    ItemError::ItemTooLarge { .. } | ItemError::TooManyItems { .. } => TOO_LONG_WORDS,
    ItemError::Truncated => CLOSED_WORDS,
  };
  (error.subversion_code(), message.to_owned())
}

// The edge's own tests run a tunnel through this module's test site (`super::tests`).
#[cfg(test)]
#[path = "tests/svn.rs"]
pub(super) mod tests;
