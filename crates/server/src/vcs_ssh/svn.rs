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
//! (A-597's `vcs_policy::svn_access`, enforcing both `Allow` and `Deny` rules) "No access allowed to this
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
//! id. The hooks run where `svnserve -t` runs them (A-730, through `crate::vcs_svn::run_hook` with the tunnel's
//! repository record): start-commit at `open-root` and pre-commit at `close-edit` (after per-path and subtree
//! authorization via `crate::vcs_svn::admit_pending_changes`), whose verdicts resume the drive;
//! post-commit once the revision stands, its failure `commit-info`'s `post-commit-err`; and around the lock verbs,
//! pre-lock or pre-unlock for each path and post-lock or post-unlock once. A hook's run ends at its learned bound, by
//! itself, or once the edge's signal says the client has gone (`super::site::ClientGone`), which ends the tunnel as
//! `svnserve` ends where its client leaves inside a command (E210002); Windows, with no hook driver, refuses a change at
//! a configured point. The work the drive accepts lands through the owner's
//! commit step ([`SvnStores::commit`]: A-655 part 2b's `repository::svn_commit::land_work`, the one both ra_svn
//! transports call, A-698), and `close-edit` is answered with `commit-info` (the revision, its date and author) or with
//! the step's refusal as its failure, after which the client's commands are dropped until its `abort-edit`.
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
//! `SvnStores`), which lends and commits on the owner shard; the
//! FSFS-layout view stock `svnlook` reads REPOS through (A-645); CRAM-MD5 through the tunnel,
//! whose keys the record of credentials does not hold yet (a lender that lists it gets every challenge failed, as
//! `svnserve` fails one it cannot make); a URL canonicalized as `svn_uri_canonicalize` would (stock clients send
//! canonical URLs); the drop's ending above.

use slates_db::catalog::{Principal, RepositoryId, RepositoryRecord, VcsTool};
use slates_svn::commit::drive::{Due, Green, HeldTexts, NeedText, TextSink, Work};
use slates_svn::commit::record::Record;
use slates_svn::find::{Found as FoundUrl, find_repository};
use slates_svn::handshake::{self, Access, Repository, Unserved as Refusal};
use slates_svn::ra_svn::ItemError;
use slates_svn::reads;
use slates_svn::report::{self, exchange::Budget};
use slates_svn::serve::{self, CommitStart, Connection, Limits, Made, Sources, Turn, Waiting};

use super::site::{
  ClientGone, Command, Next, Refused, SVNSERVE_FAILURE_STATUS, Service, Services, Start, Stepped,
  Unserved, not_served,
};
use crate::repository::svn_commit::{self, Committed};
use crate::vcs_hooks::{self, CommittedView, HookPoint, LockToken, PendingView};
use crate::vcs_http::git::Repositories;
use crate::vcs_policy::{self, SvnAccess};
use crate::vcs_store::svn_view::{BroughtText, FetchedPiece};
use crate::vcs_svn::{
  HookSite, Hooked, Located, Location, MERGEINFO, NOT_AUTHORIZED, admit_pending_changes,
  authorized_pre_lock_hooks, post_lock_hook, revision_property_hook, run_hook,
};

/// Format: the refusal count of a command the connection answered "Unknown editor command" because slates does not
/// serve it yet (`Turn::Unserved`).
const UNSERVED_COMMAND: &str = "vcs_ssh.svn_unserved_command";
/// Format: the refusal count of a session whose repository's store is not held here, or went away.
const STORE_NOT_HELD: &str = "vcs_ssh.svn_store_not_held";
/// Format: the refusal count of a session refused "No access allowed to this repository" by the repository's policy.
const NO_ACCESS: &str = "vcs_ssh.svn_no_access";
/// Format: the refusal count of a tunnel ended with no words: where `svnserve` aborts, or on the tunnel's own error.
const UNWORDED_END: &str = "vcs_ssh.svn_unworded_end";
/// Format: `SVN_ERR_FS_LOCK_OPERATION_FAILED` (`svn_error_codes.h`: the filesystem category's start, 160,000, plus 59).
const LOCK_OPERATION_FAILED: u32 = 160_059;
/// Format: the refusal count of an accepted work the owner's commit step refused.
const COMMIT_REFUSED: &str = "vcs_ssh.svn_commit_refused";

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

/// The words of a client that left where the connection still read (recorded, the module doc).
const CLOSED_WORDS: &str = "Network connection closed unexpectedly";
/// The words of bytes that are no item (`marshal.c`; recorded).
const MALFORMED_WORDS: &str = "Malformed network data";
/// The words of an item past its bound: `SVN_ERR_RA_SVN_REQUEST_SIZE`'s own (slates-svn's `report::Refused`).
const TOO_LONG_WORDS: &str = "Client request too long";

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
  type Green<'a>: reads::Repository + Green;
  /// Whether the store of repository `id` is held here.
  fn holds(&self, id: RepositoryId) -> bool;
  /// Whether repository `id`'s backing store failed recovery on this shard (`VcsStores::unrecovered`).
  fn unrecovered(&self, _id: RepositoryId) -> bool {
    false
  }
  /// Runs `step` over what repository `id`'s sessions are opened with; none when its store is not held here.
  fn opening<T>(&mut self, id: RepositoryId, step: impl FnOnce(&Opening<'_>) -> T) -> Option<T>;
  /// Lands `work`, which `principal`'s commit made on repository `id` and the connection accepted, through the owner's
  /// commit step (`repository::svn_commit::land_work` at the owner's instant, A-698): the revision it made, or why
  /// not; none when its store is not held here.
  async fn commit(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    work: Work,
    texts: &dyn TextSink,
  ) -> Option<Result<Committed, svn_commit::Uncommitted>>;
  /// A repository's landing turn (C2, `repository::svn_commit::landing::Turn`), held from a verb's judgment to its
  /// record's landing.
  type Turn;
  /// Reads the text piece `wanted` of repository `id` through the node's changeset store, where the repository's
  /// sources read it while the piece is held (C2 stage B part 2; `FetchedPiece`, given back by its drop); none where the
  /// store could not give it, or to read where the sources hold every text in memory (the default).
  async fn fetch(
    &mut self,
    id: RepositoryId,
    wanted: slates_svn::reads::Wanted,
  ) -> Option<FetchedPiece> {
    let _ = (id, wanted);
    None
  }
  /// Brings back a landed base text `need` of repository `id` whole into this shard's content store for a commit's
  /// text delta (`BroughtText::bring`); none where the store cannot give it or all texts stay in memory.
  async fn bring(&mut self, id: RepositoryId, need: NeedText) -> Option<BroughtText> {
    let _ = (id, need);
    None
  }
  /// Locates which shard holds the repository named by `found` across the daemon's partitions (`vcs_svn::locate`);
  /// `None` in single-shard test lenders that only look up in `SvnCommand::repositories`.
  async fn locate(&mut self, found: &FoundUrl) -> Option<Location> {
    let _ = found;
    None
  }
  /// Repository `id`'s landing turn, once no other landing holds it; none when its store is not held here.
  async fn turn(&mut self, id: RepositoryId) -> Option<Self::Turn>;
  /// Lands `record`, which moves no version, by `principal` on repository `id` through the owner's step, holding its
  /// landing `turn`: a lock verb's entries (`Record::Locks`, A-693 part 2) or a revision's properties whole
  /// (`Record::RevisionProperties`, A-702 part 2); whether it landed; none when its store is not held here.
  async fn land_record(
    &mut self,
    turn: Self::Turn,
    id: RepositoryId,
    principal: &Principal,
    record: Record,
  ) -> Option<Result<(), svn_commit::Uncommitted>>;
  /// Where this shard shows a hook its repository (`ShardState::hook_views`, A-645 (a)).
  fn hook_views(&self) -> crate::vcs_store::svn_presenter::HookViews;
  /// Runs `step` over repository `id`'s sources at the owner's instant; none when its store is not held here.
  fn lend<T>(
    &mut self,
    id: RepositoryId,
    texts: &dyn TextSink,
    step: impl FnOnce(&Sources<'_, Self::Green<'_>>) -> T,
  ) -> Option<T>;
  /// Runs `step` over repository `id`'s sources for `principal` at the owner's instant, deriving
  /// session access (`Write`/`Read`/`None`) from the repository's policy; defaults to [`Self::lend`]
  /// for test lenders whose fixed policy is built in.
  fn lend_for<T>(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    texts: &dyn TextSink,
    step: impl FnOnce(&Sources<'_, Self::Green<'_>>) -> T,
  ) -> Option<T> {
    let _ = principal;
    self.lend(id, texts, step)
  }
  /// Compiles `principal`'s per-path read rules for repository `id` when backed by shard state;
  /// `None` lets the caller compile from its own repository catalog.
  fn read_rules(
    &self,
    _id: RepositoryId,
    _principal: &Principal,
  ) -> Option<slates_svn::authz::PrincipalRules> {
    None
  }
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

  async fn step(&mut self, gone: ClientGone<'_>) -> Result<Stepped, Refused> {
    match self {
      Self::Other(command) => command.step(gone).await,
      Self::Tunnel(command) => command.step(gone).await,
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

  fn paced(&mut self, sent: crate::vcs_store::upload::Sent) {
    match self {
      Self::Other(command) => command.paced(sent),
      Self::Tunnel(command) => command.paced(sent),
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
  /// The text piece a drive waited for and the session fetched (C2 stage B part 2), held while the drive reads on in it:
  /// given back when the drive waits for another, when the read's answer ends, or with the session.
  fetched: Option<FetchedPiece>,
  /// A landed base text brought back whole while a commit applies delta windows against it: given back on drop, file
  /// change, exit from edit, or shard migration.
  brought: Option<BroughtText>,
  /// A repository located on another shard, carried across `Next::Move` and opened immediately on arrival on the owner
  /// shard.
  pending_open: Option<(FoundUrl, Located)>,
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
  /// Where its commits' new texts go (`slates_svn::commit::drive::TextSink`): kept in memory per session until the
  /// green's staged blocks take them (w-perf's wiring of A-716's `Green::stage`); given back at each commit's start and
  /// after its landing.
  texts: HeldTexts,
}

/// What one call of the connection made of the client's bytes, owned, so the input can be consumed after it.
enum Received {
  /// The input ends inside the client's next item.
  NeedMore,
  /// These bytes were taken and answered, or taken with nothing for the tunnel to do.
  Took(usize),
  /// Output was written and nothing taken.
  Wrote,
  /// A drive waits for a text's piece read through the node's changeset store (C2 stage B part 2).
  Waiting(slates_svn::reads::Wanted),
  /// A commit's text delta waits for its landed base text to be brought back whole.
  NeedText(NeedText),
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
  /// The client asked for a lock verb.
  LockAsked(usize),
  /// The client asked for a revision property change past its checks.
  RevisionProperty {
    /// The pre-hook's call.
    call: Box<slates_svn::serve::RevisionPropertyCall>,
    /// The bytes taken.
    consumed: usize,
  },
  /// The commit's drive waits for a hook.
  Hook {
    /// The hook.
    due: Due,
    /// The bytes taken.
    consumed: usize,
  },
  /// The commit's drive accepted its work.
  Accepted {
    /// The work.
    work: Box<Work>,
    /// The bytes taken.
    consumed: usize,
  },
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
      Turn::Waiting(wanted) => Received::Waiting(wanted),
      Turn::NeedText(need) => Received::NeedText(need),
      Turn::Opened { open, consumed } => Received::Opened {
        url: open.url.to_vec(),
        consumed,
      },
      Turn::ChallengeNeeded { consumed } => Received::Challenge(consumed),
      Turn::KeyNeeded { consumed, .. } => Received::Key(consumed),
      Turn::CommitAsked { consumed } => Received::CommitAsked(consumed),
      Turn::LockAsked { consumed } => Received::LockAsked(consumed),
      Turn::RevisionPropertyHook { call, consumed } => {
        Received::RevisionProperty { call, consumed }
      }
      Turn::Hook { due, consumed } => Received::Hook { due, consumed },
      Turn::Accepted { work, consumed } => Received::Accepted { work, consumed },
    }
  }
}

/// Result of resolving a commit hook step before resuming `Connection::answer_hook`.
enum DueAnswer {
  /// Resume the commit drive with this hook verdict (`Ok(())` or `Err(chain)`).
  Resume(Result<(), Vec<(u32, String)>>),
  /// The step already ended the tunnel (missing store, out-of-turn state, or client gone).
  Ended,
}

/// Result of authorizing pending commit changes against the live repository record and index.
enum PendingAuthz {
  /// Every changed path and affected subtree is permitted; carries the live repository policy.
  Allowed(slates_db::catalog::RepositoryPolicy),
  /// The repository record is missing, replaced, unreadable, or denies at least one path.
  Denied,
  /// The repository's Subversion store is not held on this shard.
  StoreMissing,
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
      fetched: None,
      brought: None,
      pending_open: None,
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
      texts: HeldTexts::new(),
    })
  }

  /// One step (the module doc): the connection's call over the input held, the tunnel's answer to what it asks, and the
  /// bytes it took given back.
  async fn stepped(&mut self, gone: ClientGone<'_>) -> Stepped {
    if let Some(status) = self.exit {
      self.fetched = None;
      self.brought = None;
      return exited(status);
    }
    if let Some((found_url, located)) = self.pending_open.take() {
      self.open_located(&found_url, &located);
      return self.consumed(0);
    }
    let received = self.received();
    // The piece the last wait fetched stays held while the read goes on, since a call reads only a quantum of the
    // drive's steps in it (goal 93); its answer ended, the piece goes.
    if !self.connection.fetching() {
      self.fetched = None;
    }
    if !matches!(self.connection.waiting(), Waiting::Edit) {
      self.brought = None;
    }
    self.dispatch_received(received, gone).await
  }

  /// Dispatches one outcome of [`Self::received`] and returns the next step.
  async fn dispatch_received(&mut self, received: Received, gone: ClientGone<'_>) -> Stepped {
    let consumed = match received {
      Received::NeedMore => return self.wait_or_end(),
      Received::Wrote => 0,
      Received::Waiting(wanted) => {
        self.fetch_piece(wanted).await;
        0
      }
      Received::NeedText(need) => {
        self.bring_text(need).await;
        0
      }
      Received::Took(consumed) => consumed,
      Received::Opened { url, consumed } => return self.handle_open(&url, consumed).await,
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
      Received::LockAsked(consumed) => {
        self.lock_verb(gone).await;
        consumed
      }
      Received::RevisionProperty { call, consumed } => {
        self.revision_property(&call, gone).await;
        consumed
      }
      Received::Hook { due, consumed } => {
        self.answer_hook(&due, gone).await;
        consumed
      }
      Received::Accepted { work, consumed } => {
        self.land(*work, gone).await;
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

  /// Fetches one let-go read piece for the active repository, ending the session if absent.
  async fn fetch_piece(&mut self, wanted: reads::Wanted) {
    // The drive moved on from the piece held to another.
    self.fetched = None;
    let Some(id) = self.opened.as_ref().map(|record| record.id) else {
      self.not_held();
      return;
    };
    // A piece the node's store cannot give ends the session: the drive would wait for it again.
    match self.stores.fetch(id, wanted).await {
      Some(piece) => self.fetched = Some(piece),
      None => self.not_held(),
    }
  }

  /// Brings back one let-go base text for an edit drive, ending the session if absent.
  async fn bring_text(&mut self, need: NeedText) {
    self.brought = None;
    let Some(id) = self.opened.as_ref().map(|record| record.id) else {
      self.not_held();
      return;
    };
    match self.stores.bring(id, need).await {
      Some(brought) => self.brought = Some(brought),
      None => self.not_held(),
    }
  }

  /// Opens the repository at `url`, migrating the tunnel to its owning shard when it lives elsewhere.
  async fn handle_open(&mut self, url: &[u8], consumed: usize) -> Stepped {
    if let Some(shard) = self.open(url).await {
      let taken = consumed.min(self.input.len());
      self.input.drain(..taken);
      return Stepped {
        consumed: taken,
        next: Next::Move(shard),
      };
    }
    self.consumed(consumed)
  }

  /// The connection's call over the input held: with the repository's sources once the session is established.
  fn received(&mut self) -> Received {
    let budget = crate::vcs_svn::timed(self.bounds.budget);
    let Self {
      connection,
      repositories,
      stores,
      principal,
      texts,
      opened,
      input,
      out,
      ..
    } = self;
    if connection.session().is_none() {
      let sources: Option<&Sources<'_, V::Green<'_>>> = None;
      return connection
        .receive(sources, input, out, budget)
        .map_or_else(Received::Ended, Received::of);
    }
    let Some(record) = opened.as_ref() else {
      return Received::NotHeld;
    };
    let rules = stores.read_rules(record.id, principal).unwrap_or_else(|| {
      let live = repositories.find(&record.name);
      vcs_policy::svn_read_rules_for(live.as_ref(), principal)
    });
    connection.set_read_rules(Some(rules));
    stores
      .lend_for(record.id, principal, texts, |sources| {
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

  /// Answers the client's `url` (the module doc's session): locates the repository across shards when the lender
  /// supports multi-shard lookup, returning `Some(shard)` to migrate the SSH connection to the owner shard when owned
  /// elsewhere; otherwise opens it on this shard or refuses the URL.
  async fn open(&mut self, url: &[u8]) -> Option<u16> {
    let found_url = match find_repository(url) {
      Ok(found_url) => found_url,
      Err(refusal) => {
        self.refuse_url(refusal);
        return None;
      }
    };
    if let Some(location) = self.stores.locate(&found_url).await {
      match location {
        Location::Found(located) => {
          let here = crate::state::with_state(|state| state.shard);
          if here != Some(located.shard) {
            self.fetched = None;
            self.brought = None;
            let target = located.shard;
            self.pending_open = Some((found_url, located));
            return Some(target);
          }
          self.open_located(&found_url, &located);
          return None;
        }
        Location::Nowhere => {
          self.refuse_url(Refusal::NoRepository { url });
          return None;
        }
        Location::Unanswered => {
          self.not_held();
          return None;
        }
      }
    }
    let (record, root) = match found(&self.repositories, url) {
      Found::Repository { record, root } => (*record, root),
      Found::BadUrl => {
        self.refuse_url(Refusal::BadUrl { url });
        return None;
      }
      Found::BadPath => {
        self.refuse_url(Refusal::BadPath);
        return None;
      }
      Found::NoRepository => {
        self.refuse_url(Refusal::NoRepository { url });
        return None;
      }
    };
    let root_url = url.get(..root).unwrap_or(url);
    self.open_record(record, root_url);
    None
  }

  /// Opens the repository `located` on its owner shard after `locate` (and any cross-shard migration).
  fn open_located(&mut self, found_url: &FoundUrl, located: &Located) {
    let Some(record) = self
      .repositories
      .find(&located.name)
      .filter(|record| record.tool == VcsTool::Subversion)
    else {
      self.refuse_url(Refusal::NoRepository {
        url: &found_url.url,
      });
      return;
    };
    let root_url = found_url.root_url(located.depth);
    self.open_record(record, &root_url);
  }

  /// Checks policy and store presence on the owner shard and opens `record` with `root_url`.
  fn open_record(&mut self, record: RepositoryRecord, root_url: &[u8]) {
    if !matches!(
      vcs_policy::svn_access(&record, &self.principal),
      Ok(SvnAccess::Read | SvnAccess::Write)
    ) {
      crate::fleet::count_refusal(NO_ACCESS);
      return self.refuse_url(Refusal::NoAccess);
    }
    if !self.stores.holds(record.id) {
      return self.missing_store(record.id, &record.name);
    }
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
      None => self.missing_store(record.id, &record.name),
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

  /// Judges the lock verb the client asked for over the lent sources, lands its entries through the owner's step and
  /// answers it (A-693 part 2): each new token drawn from the system's generator, one it cannot give refusing the
  /// landing; the table holding at most as many locks as one item has bytes.
  async fn lock_verb(&mut self, gone: ClientGone<'_>) {
    let Some(record) = self.opened.as_ref() else {
      return self.not_held();
    };
    let id = record.id;
    let policy = record.policy.clone();
    let hooked = Hooked::new(id, &record.name, self.stores.hook_views());
    let room = self.bounds.limits.item_bytes;
    let called = {
      let Self {
        connection,
        stores,
        principal,
        texts,
        out,
        ..
      } = self;
      stores.lend_for(id, principal, texts, |sources| {
        connection.lock_verb(sources, out)
      })
    };
    let call = match called {
      None => return self.not_held(),
      Some(Err(refused)) => return self.end(refused),
      Some(Ok(None)) => return,
      Some(Ok(Some(call))) => call,
    };
    let live = self.repositories.find(&record.name);
    let site = HookSite::Tunnel(&policy, gone);
    // No verdict: the client left during a run (the module doc's hooks).
    let Some(verdicts) =
      authorized_pre_lock_hooks(live.as_ref(), &self.principal, site, &hooked, room, &call).await
    else {
      return self.client_left();
    };
    // The landing turn, held from the judgment to the entries' landing (C2): no other landing moves the lock table
    // between them.
    let Some(turn) = self.stores.turn(id).await else {
      return self.not_held();
    };
    let undrawn = std::cell::Cell::new(false);
    let mut draw = || {
      let mut token = [0u8; slates_svn::lock::UUID_BYTES];
      if aws_lc_rs::rand::fill(&mut token).is_err() {
        undrawn.set(true);
      }
      token
    };
    let judged = {
      let Self {
        connection,
        stores,
        principal,
        texts,
        ..
      } = self;
      stores.lend_for(id, principal, texts, |sources| {
        connection.lock_judged(sources, verdicts, &mut draw, room)
      })
    };
    let entries = match judged {
      None => return self.not_held(),
      Some(Err(refused)) => return self.end(refused),
      Some(Ok(entries)) => entries,
    };
    let landed = if undrawn.get() {
      Err((
        LOCK_OPERATION_FAILED,
        "No lock token could be drawn".to_owned(),
      ))
    } else if entries.is_empty() {
      Ok(())
    } else {
      match self
        .stores
        .land_record(turn, id, &self.principal, Record::Locks(entries))
        .await
      {
        None => return self.not_held(),
        Some(landed) => landed.map_err(|refused| refused.failure()),
      }
    };
    let post = match self.connection.locks_landed(landed, &mut self.out) {
      Ok(post) => post,
      Err(refused) => return self.end(refused),
    };
    let ended = match post {
      Some(call) => match post_lock_hook(site, &hooked, room, &call).await {
        Some(ended) => ended,
        None => return self.client_left(),
      },
      None => Ok(()),
    };
    let answered = self.connection.locks_finished(ended, &mut self.out);
    self.end_if_refused(answered);
  }

  /// Runs the client's revision property change past its checks (A-702 part 2), `call` its pre-hook's, as the listener
  /// does (`vcs_svn::revision_property`): pre-revprop-change, then the change judged over the lent sources and its
  /// record landed through the owner's step, then post-revprop-change with the old value.
  async fn revision_property(
    &mut self,
    call: &slates_svn::serve::RevisionPropertyCall,
    gone: ClientGone<'_>,
  ) {
    let Some(record) = self.opened.as_ref() else {
      return self.not_held();
    };
    let id = record.id;
    let policy = record.policy.clone();
    let hooked = Hooked::new(id, &record.name, self.stores.hook_views());
    let room = self.bounds.limits.item_bytes;
    let site = HookSite::Tunnel(&policy, gone);
    // No verdict: the client left during a run (the module doc's hooks).
    let pre = HookPoint::SvnPreRevpropChange;
    let Some(verdict) = revision_property_hook(site, &hooked, room, pre, call).await else {
      return self.client_left();
    };
    // The landing turn, held from the judgment to the record's landing (C2): no other change lands in between.
    let Some(turn) = self.stores.turn(id).await else {
      return self.not_held();
    };
    let judged = {
      let Self {
        connection,
        stores,
        principal,
        texts,
        out,
        ..
      } = self;
      stores.lend_for(id, principal, texts, |sources| {
        connection.revision_property_hooked(sources, verdict, out)
      })
    };
    let record = match judged {
      None => return self.not_held(),
      Some(Err(refused)) => return self.end(refused),
      Some(Ok(None)) => return,
      Some(Ok(Some(record))) => record,
    };
    let landed = match self
      .stores
      .land_record(turn, id, &self.principal, record)
      .await
    {
      None => return self.not_held(),
      Some(landed) => landed.map_err(|refused| refused.failure()),
    };
    let post = match self
      .connection
      .revision_property_landed(landed, &mut self.out)
    {
      Ok(Some(post)) => post,
      Ok(None) => return,
      Err(refused) => return self.end(refused),
    };
    let post_point = HookPoint::SvnPostRevpropChange;
    let Some(ended) = revision_property_hook(site, &hooked, room, post_point, &post).await else {
      return self.client_left();
    };
    let answered = self
      .connection
      .revision_property_finished(ended, &mut self.out);
    self.end_if_refused(answered);
  }

  /// Begins the commit the client asked for, its transaction named as the module doc says, over the lent sources.
  fn begin_commit(&mut self) {
    self.commits = self.commits.saturating_add(1);
    // A drive that ended without landing (an abort, a refusal) leaves its texts: given back as the next commit starts.
    self.texts.clear();
    let commits = self.commits;
    let room = self.bounds.commit_room;
    let Some(id) = self.opened.as_ref().map(|record| record.id) else {
      return self.not_held();
    };
    let Self {
      connection,
      stores,
      principal,
      texts,
      out,
      ..
    } = self;
    let begun = stores.lend_for(id, principal, texts, |sources| {
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

  /// Authorizes pending commit `changes` at `base` against the live repository record and index
  /// before invoking `HookPoint::SvnPreCommit`.
  fn authorize_pending(
    &mut self,
    id: RepositoryId,
    name: &str,
    base: slates_svn::index::Revision,
    changes: &[slates_svn::index::ChangedPath],
  ) -> PendingAuthz {
    let Some(live) = self.repositories.find(name).filter(|repo| repo.id == id) else {
      return PendingAuthz::Denied;
    };
    let Self {
      stores,
      principal,
      texts,
      ..
    } = self;
    let admitted = stores.lend_for(id, principal, texts, |sources| {
      admit_pending_changes(&live, principal, sources.index, base, changes)
    });
    match admitted {
      None => PendingAuthz::StoreMissing,
      Some(Ok(())) => PendingAuthz::Allowed(live.policy),
      Some(Err(_)) => PendingAuthz::Denied,
    }
  }

  /// Runs `HookPoint::SvnStartCommit` for the pending `open-root` transaction.
  async fn start_commit_answer(&mut self, transaction: &str, gone: ClientGone<'_>) -> DueAnswer {
    let Some(record) = self.opened.as_ref() else {
      self.not_held();
      return DueAnswer::Ended;
    };
    let policy = record.policy.clone();
    let hooked = Hooked::new(record.id, &record.name, self.stores.hook_views());
    let user = String::from_utf8_lossy(&self.user).into_owned();
    let mergeinfo = self
      .connection
      .session()
      .is_some_and(|session| session.capabilities.mergeinfo);
    let base = self.connection.pending().map_or(0, |pending| pending.base);
    let capabilities: &[&str] = if mergeinfo { &[MERGEINFO] } else { &[] };
    let words_cap = self.bounds.limits.item_bytes;
    let ran = run_hook(
      HookSite::Tunnel(&policy, gone),
      &hooked,
      words_cap,
      HookPoint::SvnStartCommit,
      |env, repos| {
        vcs_hooks::svn_start_commit(
          PendingView(base),
          env,
          repos,
          &user,
          capabilities,
          transaction,
        )
      },
    )
    .await;
    match ran.verdict {
      Some(verdict) => DueAnswer::Resume(verdict),
      None => {
        self.texts.clear();
        self.client_left();
        DueAnswer::Ended
      }
    }
  }

  /// Authorizes pending commit changes (including copy-from sources and deleted/replaced
  /// directory subtrees) against the live repository record and index BEFORE invoking
  /// `HookPoint::SvnPreCommit`, refusing unauthorized commits with `E170001` without running
  /// the hook.
  async fn pre_commit_answer(&mut self, transaction: &str, gone: ClientGone<'_>) -> DueAnswer {
    let Some(record) = self.opened.as_ref() else {
      self.not_held();
      return DueAnswer::Ended;
    };
    let id = record.id;
    let name = record.name.clone();
    let hooked = Hooked::new(id, &name, self.stores.hook_views());
    let Some(pending) = self.connection.pending() else {
      self.texts.clear();
      self.end(serve::Refused::OutOfTurn);
      return DueAnswer::Ended;
    };
    let base = pending.base;
    let changes: Vec<slates_svn::index::ChangedPath> = pending.rows.values().cloned().collect();
    let tokens: Vec<LockToken> = pending
      .lock_tokens()
      .into_iter()
      .map(|(path, token)| LockToken { path, token })
      .collect();
    let policy = match self.authorize_pending(id, &name, base, &changes) {
      PendingAuthz::Allowed(policy) => policy,
      PendingAuthz::Denied => {
        self.texts.clear();
        return DueAnswer::Resume(Err(vec![(
          NOT_AUTHORIZED,
          "Authorization failed".to_owned(),
        )]));
      }
      PendingAuthz::StoreMissing => {
        self.texts.clear();
        self.not_held();
        return DueAnswer::Ended;
      }
    };
    let words_cap = self.bounds.limits.item_bytes;
    let ran = run_hook(
      HookSite::Tunnel(&policy, gone),
      &hooked,
      words_cap,
      HookPoint::SvnPreCommit,
      |env, repos| vcs_hooks::svn_pre_commit(PendingView(base), env, repos, transaction, &tokens),
    )
    .await;
    match ran.verdict {
      Some(verdict) => DueAnswer::Resume(verdict),
      None => {
        self.texts.clear();
        self.client_left();
        DueAnswer::Ended
      }
    }
  }

  /// Answers the hook the drive waits for (the module doc's commit): `start-commit` at
  /// `open-root`, and `pre-commit` at `close-edit` AFTER per-path and subtree authorization
  /// ([`Self::pre_commit_answer`]). Its verdict resumes the drive, and an accepted work lands
  /// through [`Self::land`], which re-checks live authorization before committing.
  async fn answer_hook(&mut self, due: &Due, gone: ClientGone<'_>) {
    let due_answer = match due {
      Due::StartCommit { transaction } => self.start_commit_answer(transaction, gone).await,
      Due::PreCommit { transaction } => self.pre_commit_answer(transaction, gone).await,
    };
    let answer = match due_answer {
      DueAnswer::Resume(verdict) => verdict,
      DueAnswer::Ended => return,
    };
    let Some(id) = self.opened.as_ref().map(|record| record.id) else {
      self.texts.clear();
      return self.not_held();
    };
    let Self {
      connection,
      stores,
      principal,
      texts,
      out,
      ..
    } = self;
    let answered = stores.lend_for(id, principal, texts, |sources| {
      connection.answer_hook(answer, sources, out)
    });
    match answered {
      None => {
        self.texts.clear();
        self.not_held();
      }
      Some(Ok(Turn::Accepted { work, .. })) => Box::pin(self.land(*work, gone)).await,
      Some(Ok(_)) => {}
      Some(Err(refused)) => self.end(refused),
    }
  }

  /// Refuses an accepted commit with `E170001` ("Authorization failed") and clears staged texts.
  fn refuse_uncommitted(&mut self) {
    self.texts.clear();
    let refused = slates_svn::serve::Uncommitted::Refused {
      code: NOT_AUTHORIZED,
      message: "Authorization failed".to_owned(),
    };
    if let Err(refused) = self.connection.not_committed(&refused, &mut self.out) {
      self.end(refused);
    }
  }

  /// Lands the accepted `work` through the owner's commit step (the module doc's commit) and answers `close-edit` with
  /// the revision it made, or with its refusal.
  async fn land(&mut self, work: Work, gone: ClientGone<'_>) {
    let Some(record) = self.opened.as_ref() else {
      return self.not_held();
    };
    let id = record.id;
    let name = record.name.clone();
    let hooked = Hooked::new(id, &name, self.stores.hook_views());
    let Some(live) = self.repositories.find(&name) else {
      return self.refuse_uncommitted();
    };
    let policy = live.policy.clone();
    let admitted = {
      let Self {
        stores,
        principal,
        texts,
        ..
      } = self;
      stores.lend_for(id, principal, texts, |sources| {
        vcs_policy::admit_svn_changes(
          &live.owner,
          &live.policy,
          principal,
          (sources.index, work.head),
          &work.record.changes,
        )
      })
    };
    match admitted {
      None => return self.not_held(),
      Some(Err(_)) => return self.refuse_uncommitted(),
      Some(Ok(())) => {}
    }
    let transaction = work.record.transaction.clone();
    let landed = self
      .stores
      .commit(id, &self.principal, work, &self.texts)
      .await;
    self.texts.clear();
    let Some(landed) = landed else {
      return self.not_held();
    };
    // post-commit runs once the revision stands, and its failure is `commit-info`'s `post-commit-err` (A-730).
    let post_commit = match &landed {
      Ok(committed) => {
        let revision = committed.revision;
        let words_cap = self.bounds.limits.item_bytes;
        run_hook(
          HookSite::Tunnel(&policy, gone),
          &hooked,
          words_cap,
          HookPoint::SvnPostCommit,
          |env, repos| {
            vcs_hooks::svn_post_commit(CommittedView(revision), env, repos, revision, &transaction)
          },
        )
        .await
        .post_commit_error
      }
      Err(_) => None,
    };
    let answered = match &landed {
      // A client's work always carries the date the step gave it (`Revision::of_work`).
      Ok(committed) => self.connection.committed(
        &Made {
          revision: committed.revision,
          date: committed.date.as_deref().unwrap_or_default(),
          author: committed.author.as_deref(),
          post_commit_error: post_commit.as_deref(),
        },
        &mut self.out,
      ),
      Err(refused) => {
        crate::fleet::count_refusal(COMMIT_REFUSED);
        self
          .connection
          .not_committed(&refused.answered(), &mut self.out)
      }
    };
    self.end_if_refused(answered);
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

  /// Ends the tunnel because its client left while a hook ran (the module doc's hooks): as `svnserve` ends where its
  /// client leaves inside a command, though nothing written reaches a client that is gone.
  fn client_left(&mut self) {
    self.ends_with(CONNECTION_CLOSED, CLOSED_WORDS.as_bytes());
  }

  /// Handles a missing backing store during [`Self::open_record`]: emits typed `E160004` when
  /// recovery failed for `id`, otherwise falls back to the site's not-served refusal.
  fn missing_store(&mut self, id: RepositoryId, name: &str) {
    if self.stores.unrecovered(id) {
      self.store_unrecovered(name);
    } else {
      self.not_held();
    }
  }

  /// Ends the tunnel with typed `E160004` (`SVN_ERR_FS_CORRUPT`) on stdout and stderr for an
  /// unrecovered or missing backing store of repository `name`.
  fn store_unrecovered(&mut self, name: &str) {
    crate::fleet::count_refusal(STORE_NOT_HELD);
    let message = handshake::store_unrecovered_message(name);
    if self
      .connection
      .fail_command(handshake::SVN_ERR_FS_CORRUPT, &message, &mut self.out)
      .is_err()
    {
      crate::fleet::count_refusal(UNWORDED_END);
    }
    self.ends_with(handshake::SVN_ERR_FS_CORRUPT, message.as_bytes());
  }

  /// Ends the tunnel because the repository's store is not held here: mid-session (once a
  /// repository is open) emits a typed `E160004` (`SVN_ERR_FS_CORRUPT`) failure frame on stdout
  /// and `svnserve: E160004: ...` on stderr; before open falls back to the site's not-served words.
  fn not_held(&mut self) {
    if let Some(name) = self.opened.as_ref().map(|record| record.name.clone()) {
      self.store_unrecovered(&name);
      return;
    }
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

  async fn step(&mut self, gone: ClientGone<'_>) -> Result<Stepped, Refused> {
    Ok(self.stepped(gone).await)
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
    /// The repository, boxed: no other answer carries anything.
    record: Box<RepositoryRecord>,
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
      Some(Found::Repository {
        record: Box::new(record),
        root,
      })
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
pub(crate) fn transaction_name(base: u64, count: u64) -> String {
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

#[cfg(test)]
mod store_unrecovered_tests {
  use std::cell::Cell;
  use std::future::Future;
  use std::task::{Context, Poll, Waker};

  use slates_db::HostId;
  use slates_db::catalog::{
    ObjectFormat, Principal, RepositoryId, RepositoryPolicy, RepositoryRecord, VcsTool,
  };
  use slates_svn::authz;
  use slates_svn::commit::drive::{TextSink, Work};
  use slates_svn::commit::record::Record;
  use slates_svn::dump::Uuid;
  use slates_svn::lock::AprTime;
  use slates_svn::migration::{Repository as Records, Room};
  use slates_svn::report::exchange::Budget;
  use slates_svn::serve::{Limits, Policy, Sources};

  use super::{Opening, SvnCommand, SvnStores, TunnelBounds};
  use crate::repository::svn_commit::{Committed, Uncommitted as Unlanded};
  use crate::vcs_http::git::{Located, Repositories};
  use crate::vcs_ssh::site::{ClientGone, Command, Next};

  const REPO_ID: RepositoryId = RepositoryId { bytes: [9; 16] };
  const OWNER_UID: u32 = 77;
  const TEST_UUID: &str = "00000000-0000-4000-8000-00000000a617";

  fn owner_principal() -> Principal {
    Principal::Uid {
      host: HostId(0),
      uid: OWNER_UID,
    }
  }

  fn ready<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
      Poll::Ready(out) => out,
      Poll::Pending => panic!("expected immediate completion"),
    }
  }

  struct OneRepo;

  impl Repositories for OneRepo {
    fn locate(&self, _name: &str) -> Located {
      Located::Here
    }
    fn find(&self, name: &str) -> Option<RepositoryRecord> {
      if name != "demo" {
        return None;
      }
      Some(RepositoryRecord {
        index_bits: 0,
        id: REPO_ID,
        name: "demo".to_owned(),
        tool: VcsTool::Subversion,
        format: ObjectFormat::Revisions,
        default_branch: None,
        owner: owner_principal(),
        policy: RepositoryPolicy {
          version: 1,
          rules: Vec::new(),
          hooks: Vec::new(),
          hooks_env: Vec::new(),
          signers: Vec::new(),
          hg_publishing: true,
        },
      })
    }
  }

  struct DroppableStore {
    alive: &'static Cell<bool>,
    unrecovered: bool,
    records: &'static Records,
    record: Option<RepositoryRecord>,
  }

  impl SvnStores for DroppableStore {
    type Green<'a> = Records;
    type Turn = ();

    fn holds(&self, id: RepositoryId) -> bool {
      id == REPO_ID && self.alive.get()
    }

    fn unrecovered(&self, id: RepositoryId) -> bool {
      id == REPO_ID && self.unrecovered
    }

    fn opening<T>(&mut self, id: RepositoryId, step: impl FnOnce(&Opening<'_>) -> T) -> Option<T> {
      self.holds(id).then(|| {
        step(&Opening {
          uuid: TEST_UUID.as_bytes(),
          realm: TEST_UUID.as_bytes(),
          mergeinfo: true,
          anonymous: false,
          authenticated: true,
          cram_md5: false,
        })
      })
    }

    async fn commit(
      &mut self,
      _id: RepositoryId,
      _principal: &Principal,
      _work: Work,
      _texts: &dyn TextSink,
    ) -> Option<Result<Committed, Unlanded>> {
      None
    }

    async fn turn(&mut self, id: RepositoryId) -> Option<()> {
      self.holds(id).then_some(())
    }

    async fn land_record(
      &mut self,
      _turn: (),
      id: RepositoryId,
      _principal: &Principal,
      _record: Record,
    ) -> Option<Result<(), Unlanded>> {
      (self.record.is_some() && self.holds(id)).then_some(Ok(()))
    }

    fn hook_views(&self) -> crate::vcs_store::svn_presenter::HookViews {
      crate::vcs_store::svn_presenter::HookViews::default()
    }

    fn lend<T>(
      &mut self,
      id: RepositoryId,
      texts: &dyn TextSink,
      step: impl FnOnce(&Sources<'_, Records>) -> T,
    ) -> Option<T> {
      if !self.holds(id) {
        return None;
      }
      Some(step(&Sources {
        index: self.records.index(),
        repository: self.records,
        locks: self.records.locks(),
        now: AprTime(0),
        policy: Policy {
          anonymous: authz::Access::None,
          authenticated: authz::Access::Write,
          authz: None,
          filesystem: "demo",
        },
        texts,
      }))
    }

    fn lend_for<T>(
      &mut self,
      id: RepositoryId,
      principal: &Principal,
      texts: &dyn TextSink,
      step: impl FnOnce(&Sources<'_, Records>) -> T,
    ) -> Option<T> {
      if !self.holds(id) {
        return None;
      }
      let authenticated = match self.record.as_ref() {
        Some(record) => crate::vcs_store::svn_view::tunnel_access(Some(record), principal),
        None => authz::Access::Write,
      };
      Some(step(&Sources {
        index: self.records.index(),
        repository: self.records,
        locks: self.records.locks(),
        now: AprTime(0),
        policy: Policy {
          anonymous: authz::Access::None,
          authenticated,
          authz: None,
          filesystem: "demo",
        },
        texts,
      }))
    }
  }

  fn test_records() -> &'static Records {
    Box::leak(Box::new(
      Records::new(
        Uuid::parse(TEST_UUID.as_bytes()).unwrap(),
        b"2026-11-19T00:00:00.000000Z",
        Room {
          entries: 1 << 10,
          bytes: 1 << 20,
        },
      )
      .unwrap(),
    ))
  }

  fn test_bounds() -> TunnelBounds {
    TunnelBounds {
      limits: Limits {
        item_bytes: 1 << 20,
        report_room: 1 << 20,
        most_open: 64,
      },
      budget: Budget {
        room: 1 << 20,
        steps: 1 << 16,
        quantum: None,
      },
      commit_room: 1 << 20,
    }
  }

  fn drive_until_exit<R: Repositories, V: SvnStores>(
    cmd: &mut SvnCommand<R, V>,
  ) -> (u32, String, String) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit_code = loop {
      let step = ready(cmd.step(ClientGone::never())).unwrap();
      let (err, out) = cmd.output();
      stderr.extend_from_slice(err);
      stdout.extend_from_slice(out);
      let (err_len, out_len) = (err.len(), out.len());
      cmd.sent(err_len, out_len);
      if let Next::Exited(code) = step.next {
        break code;
      }
    };
    (
      exit_code,
      String::from_utf8_lossy(&stdout).into_owned(),
      String::from_utf8_lossy(&stderr).into_owned(),
    )
  }

  fn drive_until_input<R: Repositories, V: SvnStores>(cmd: &mut SvnCommand<R, V>) -> String {
    let mut stdout = Vec::new();
    loop {
      let step = ready(cmd.step(ClientGone::never())).unwrap();
      let (err, out) = cmd.output();
      stdout.extend_from_slice(out);
      let (err_len, out_len) = (err.len(), out.len());
      cmd.sent(err_len, out_len);
      if matches!(step.next, Next::Input) {
        return String::from_utf8_lossy(&stdout).into_owned();
      }
      assert!(
        !matches!(step.next, Next::Exited(_)),
        "expected session to wait for input, exited early"
      );
    }
  }

  /// Do establish an `svn+ssh://` tunnel session over `SvnCommand`, drop the backing store
  /// mid-session, and send a `get-latest-rev` command. Expect: the tunnel emits a typed `E160004`
  /// (`SVN_ERR_FS_CORRUPT`) failure frame on stdout AND `svnserve: E160004: Repository 'demo'
  /// store is unrecovered or corrupt` on stderr, exiting 1 rather than dropping silently.
  #[test]
  fn mid_session_store_loss_over_ssh_emits_typed_e160004_on_stdout_and_stderr() {
    let alive: &'static Cell<bool> = Box::leak(Box::new(Cell::new(true)));
    let records = test_records();
    let mut cmd = SvnCommand::new(
      OneRepo,
      DroppableStore {
        alive,
        unrecovered: false,
        records,
        record: None,
      },
      &owner_principal(),
      test_bounds(),
    )
    .expect("tunnel starts");

    let handshake =
      b"( 2 ( edit-pipeline svndiff1 absent-entries depth mergeinfo log-revprops ) 22:svn+ssh://fixture/demo 8:SVN/1.15 ( ) ) \
        ( EXTERNAL ( 0: ) ) ";
    cmd.input(handshake).unwrap();
    let _ = drive_until_input(&mut cmd);

    // Drop backing store mid-session and send a command.
    alive.set(false);
    cmd.input(b"( get-latest-rev ( ) ) ").unwrap();
    let (exit_code, stdout_str, stderr_str) = drive_until_exit(&mut cmd);
    assert_eq!(exit_code, 1);
    assert!(
      stdout_str.contains("( failure ( ( 160004 ") && stdout_str.contains("unrecovered"),
      "ssh tunnel stdout must carry E160004 frame: {stdout_str:?}"
    );
    assert!(
      stderr_str.contains("svnserve: E160004:") && stderr_str.contains("demo"),
      "ssh tunnel stderr must carry E160004 message: {stderr_str:?}"
    );
  }

  fn assert_open_unrecovered_e160004(
    alive: &'static Cell<bool>,
    records: &'static Records,
    open_msg: &[u8],
  ) {
    let mut cmd = SvnCommand::new(
      OneRepo,
      DroppableStore {
        alive,
        unrecovered: true,
        records,
        record: None,
      },
      &owner_principal(),
      test_bounds(),
    )
    .expect("tunnel starts");
    cmd.input(open_msg).unwrap();
    let (exit_code, stdout_str, stderr_str) = drive_until_exit(&mut cmd);
    assert_eq!(exit_code, 1);
    assert!(
      stdout_str.contains("( failure ( ( 160004 ") && stdout_str.contains("unrecovered"),
      "unrecovered open_record stdout must carry E160004 frame: {stdout_str:?}"
    );
    assert!(
      stderr_str.contains("svnserve: E160004:") && stderr_str.contains("demo"),
      "unrecovered open_record stderr must carry E160004 message: {stderr_str:?}"
    );
  }

  fn assert_open_unheld_not_served(
    alive: &'static Cell<bool>,
    records: &'static Records,
    open_msg: &[u8],
  ) {
    let mut cmd = SvnCommand::new(
      OneRepo,
      DroppableStore {
        alive,
        unrecovered: false,
        records,
        record: None,
      },
      &owner_principal(),
      test_bounds(),
    )
    .expect("tunnel starts");
    cmd.input(open_msg).unwrap();
    let (exit_code, _, stderr_str) = drive_until_exit(&mut cmd);
    assert_eq!(exit_code, 1);
    assert!(
      stderr_str.contains("not served yet"),
      "non-unrecovered unheld store must keep not-served refusal: {stderr_str:?}"
    );
  }

  /// Gemini Suggestion 4: Do open an `svn+ssh://` session against a repository whose backing store
  /// is not held (`holds == false`): once with `unrecovered == true` and once with
  /// `unrecovered == false`. Expect: `unrecovered == true` emits typed `E160004`
  /// (`SVN_ERR_FS_CORRUPT`) on stdout and stderr and exits 1; `unrecovered == false` preserves
  /// the generic not-served refusal without disturbing `a_repository_whose_store_is_not_held_here_is_not_served`.
  #[test]
  fn suggestion4_open_record_emits_typed_e160004_when_unrecovered_and_preserves_not_served_when_unheld()
   {
    let alive: &'static Cell<bool> = Box::leak(Box::new(Cell::new(false)));
    let records = test_records();
    let open_msg =
      b"( 2 ( edit-pipeline svndiff1 absent-entries depth mergeinfo log-revprops ) 22:svn+ssh://fixture/demo 8:SVN/1.15 ( ) ) ";
    assert_open_unrecovered_e160004(alive, records, open_msg);
    assert_open_unheld_not_served(alive, records, open_msg);
  }

  /// Shape: UID of a read-only principal (`PolicyRight::Read` only) in the `svn+ssh://` policy test.
  const READER_UID: u32 = 88;
  /// Shape: UID of a writer principal (`Read` + `Submit`) in the `svn+ssh://` policy test.
  const WRITER_UID: u32 = 89;
  /// Shape: UID of an outsider with no policy rule in the `svn+ssh://` policy test.
  const OUTSIDER_UID: u32 = 90;
  /// Shape: UID of a `Submit`-only principal (without `Read`) in the `svn+ssh://` policy test.
  const SUBMIT_ONLY_UID: u32 = 91;
  /// Shape: UID of a principal denied by explicit `PolicyEffect::Deny` in the `svn+ssh://` test.
  const DENIED_UID: u32 = 4242;

  fn svn_policy_record(rules: Vec<slates_db::catalog::PolicyRule>) -> RepositoryRecord {
    RepositoryRecord {
      index_bits: 0,
      id: REPO_ID,
      name: "demo".to_owned(),
      tool: VcsTool::Subversion,
      format: ObjectFormat::Revisions,
      default_branch: None,
      owner: owner_principal(),
      policy: RepositoryPolicy {
        version: 1,
        rules,
        hooks: Vec::new(),
        hooks_env: Vec::new(),
        signers: Vec::new(),
        hg_publishing: true,
      },
    }
  }

  fn read_only_policy_record() -> RepositoryRecord {
    use slates_db::catalog::{
      PolicyEffect, PolicyEntry, PolicyNames, PolicyRight, PolicyRule, PolicyWho,
    };
    let uid = |n| Principal::Uid {
      host: HostId(0),
      uid: n,
    };
    let rule = |right: PolicyRight, target: u32| PolicyRule {
      names: PolicyNames::Paths,
      pattern: "/".to_owned(),
      right,
      effect: PolicyEffect::Allow,
      who: PolicyWho::AnyOf {
        entries: vec![PolicyEntry::Is {
          principal: uid(target),
        }],
      },
    };
    svn_policy_record(vec![
      rule(PolicyRight::Read, READER_UID),
      rule(PolicyRight::Read, WRITER_UID),
      rule(PolicyRight::Submit, WRITER_UID),
      rule(PolicyRight::Submit, SUBMIT_ONLY_UID),
    ])
  }

  struct PolicyRepo(RepositoryRecord);

  impl Repositories for PolicyRepo {
    fn locate(&self, _name: &str) -> Located {
      Located::Here
    }
    fn find(&self, name: &str) -> Option<RepositoryRecord> {
      (name == "demo").then(|| self.0.clone())
    }
  }

  fn assert_tunnel_access_policy(record: &RepositoryRecord, reader: &Principal) {
    let uid = |n| Principal::Uid {
      host: HostId(0),
      uid: n,
    };
    assert_eq!(
      crate::vcs_store::svn_view::tunnel_access(Some(record), &owner_principal()),
      authz::Access::Write
    );
    assert_eq!(
      crate::vcs_store::svn_view::tunnel_access(Some(record), reader),
      authz::Access::Read
    );
    assert_eq!(
      crate::vcs_store::svn_view::tunnel_access(Some(record), &uid(WRITER_UID)),
      authz::Access::Write
    );
    assert_eq!(
      crate::vcs_store::svn_view::tunnel_access(Some(record), &uid(SUBMIT_ONLY_UID)),
      authz::Access::None
    );
    assert_eq!(
      crate::vcs_store::svn_view::tunnel_access(Some(record), &uid(OUTSIDER_UID)),
      authz::Access::None
    );
    assert_eq!(
      crate::vcs_store::svn_view::tunnel_access(None, &owner_principal()),
      authz::Access::None
    );
  }

  fn open_ssh_tunnel(
    record: RepositoryRecord,
    principal: &Principal,
  ) -> SvnCommand<PolicyRepo, DroppableStore> {
    let alive: &'static Cell<bool> = Box::leak(Box::new(Cell::new(true)));
    let records = test_records();
    SvnCommand::new(
      PolicyRepo(record.clone()),
      DroppableStore {
        alive,
        unrecovered: false,
        records,
        record: Some(record),
      },
      principal,
      test_bounds(),
    )
    .expect("tunnel starts")
  }

  /// Gemini Suggestion 3 (`svn+ssh://` policy): Do check `tunnel_access` across owner, reader,
  /// writer (`Read` + `Submit`), `Submit`-only, outsider, and missing record, then drive an
  /// `SvnCommand` session as the read-only principal sending `get-latest-rev` followed by `commit`.
  /// Expect: `tunnel_access` returns `Write`/`Read`/`Write`/`None`/`None`/`None`; over
  /// `SvnCommand`, `get-latest-rev` succeeds (`( success ( 0 ) )`) and `commit` is refused
  /// `E170001` (`Authorization failed`).
  #[test]
  fn suggestion3_ssh_tunnel_read_only_principal_reads_and_is_refused_commit_e170001() {
    let record = read_only_policy_record();
    let reader = Principal::Uid {
      host: HostId(0),
      uid: READER_UID,
    };
    assert_tunnel_access_policy(&record, &reader);

    let mut cmd = open_ssh_tunnel(record, &reader);
    let handshake =
      b"( 2 ( edit-pipeline svndiff1 absent-entries depth mergeinfo log-revprops ) 22:svn+ssh://fixture/demo 8:SVN/1.15 ( ) ) \
        ( EXTERNAL ( 0: ) ) ";
    cmd.input(handshake).unwrap();
    let _ = drive_until_input(&mut cmd);

    cmd.input(b"( get-latest-rev ( ) ) ").unwrap();
    let read_out = drive_until_input(&mut cmd);
    assert!(
      read_out.contains("( success ( 0 ) )"),
      "read-only ssh principal must be allowed get-latest-rev: {read_out:?}"
    );

    cmd
      .input(b"( commit ( 4:test ( ) false ( ( 7:svn:log 4:test ) ) ) ) ")
      .unwrap();
    let commit_out = drive_until_input(&mut cmd);
    assert!(
      commit_out.contains("( failure ( ( 170001 "),
      "read-only ssh principal must be refused commit with E170001: {commit_out:?}"
    );
  }

  fn assert_ssh_handshake_refused_e170001(
    record: RepositoryRecord,
    principal: &Principal,
    label: &str,
  ) {
    let open_msg =
      b"( 2 ( edit-pipeline svndiff1 absent-entries depth mergeinfo log-revprops ) 22:svn+ssh://fixture/demo 8:SVN/1.15 ( ) ) ";
    assert_eq!(
      crate::vcs_store::svn_view::tunnel_access(Some(&record), principal),
      authz::Access::None,
      "{label} tunnel_access must be None"
    );
    let mut cmd = open_ssh_tunnel(record, principal);
    cmd.input(open_msg).unwrap();
    let (exit_code, stdout, stderr) = drive_until_exit(&mut cmd);
    assert_eq!(exit_code, 1, "{label} must exit 1");
    assert!(
      stdout.contains("170001")
        && stdout.contains("No access allowed")
        && !stdout.contains("EXTERNAL"),
      "{label} must refuse E170001 without disclosing EXTERNAL/UUID: {stdout:?}"
    );
    assert!(
      stderr.contains("E170001") && stderr.contains("No access allowed"),
      "{label} stderr must carry E170001: {stderr:?}"
    );
  }

  fn assert_ssh_read_allowed_commit_refused(record: RepositoryRecord, principal: &Principal) {
    let open_msg =
      b"( 2 ( edit-pipeline svndiff1 absent-entries depth mergeinfo log-revprops ) 22:svn+ssh://fixture/demo 8:SVN/1.15 ( ) ) ";
    assert_eq!(
      crate::vcs_store::svn_view::tunnel_access(Some(&record), principal),
      authz::Access::Read
    );
    let mut cmd = open_ssh_tunnel(record, principal);
    cmd.input(open_msg).unwrap();
    cmd.input(b"( EXTERNAL ( 0: ) ) ").unwrap();
    let _ = drive_until_input(&mut cmd);
    cmd.input(b"( get-latest-rev ( ) ) ").unwrap();
    let read_out = drive_until_input(&mut cmd);
    assert!(
      read_out.contains("( success ( 0 ) )"),
      "Deny Submit must still allow reads: {read_out:?}"
    );
    cmd
      .input(b"( commit ( 4:test ( ) false ( ( 7:svn:log 4:test ) ) ) ) ")
      .unwrap();
    let commit_out = drive_until_input(&mut cmd);
    assert!(
      commit_out.contains("( failure ( ( 170001 "),
      "Deny Submit must refuse commit with E170001: {commit_out:?}"
    );
  }

  /// Round-4 Blocker 2 + Suggestion 2 (`svn+ssh://` front-door `PolicyEffect::Deny` and
  /// `Submit`-only enforcement): Do open `SvnCommand` sessions for (a) `Submit`-only without
  /// `Read`, (b) `[Allow Read / Anyone, Deny Read / uid:4242]`, and (c) `[Allow Read / Anyone,
  /// Allow Submit / Anyone, Deny Submit / uid:4242]`. Expect: (a) and (b) are refused `E170001`
  /// ("No access allowed to this repository") at handshake BEFORE disclosing the repository UUID,
  /// while (c) reads (`get-latest-rev`) cleanly and refuses `commit` with `E170001`.
  #[test]
  fn round4_blocker2_ssh_tunnel_enforces_deny_read_deny_submit_and_submit_only() {
    use slates_db::catalog::{
      PolicyEffect, PolicyEntry, PolicyNames, PolicyRight, PolicyRule, PolicyWho,
    };
    let denied = Principal::Uid {
      host: HostId(0),
      uid: DENIED_UID,
    };
    let submit_only = Principal::Uid {
      host: HostId(0),
      uid: SUBMIT_ONLY_UID,
    };
    let only_denied = PolicyWho::AnyOf {
      entries: vec![PolicyEntry::Is {
        principal: denied.clone(),
      }],
    };
    let rule = |right: PolicyRight, effect: PolicyEffect, who: PolicyWho| PolicyRule {
      names: PolicyNames::Paths,
      pattern: "/".to_owned(),
      right,
      effect,
      who,
    };

    assert_ssh_handshake_refused_e170001(read_only_policy_record(), &submit_only, "Submit-only");

    let deny_read = svn_policy_record(vec![
      rule(PolicyRight::Read, PolicyEffect::Allow, PolicyWho::Anyone),
      rule(PolicyRight::Read, PolicyEffect::Deny, only_denied.clone()),
    ]);
    assert_ssh_handshake_refused_e170001(deny_read, &denied, "Deny Read");

    let deny_write = svn_policy_record(vec![
      rule(PolicyRight::Read, PolicyEffect::Allow, PolicyWho::Anyone),
      rule(PolicyRight::Submit, PolicyEffect::Allow, PolicyWho::Anyone),
      rule(PolicyRight::Submit, PolicyEffect::Deny, only_denied),
    ]);
    assert_ssh_read_allowed_commit_refused(deny_write, &denied);
  }

  fn seeded_ssh_records() -> &'static Records {
    use slates_svn::index::{Action, ChangedPath, NodeKind, RevisionRecord, Text};
    let mut records = Records::new(
      Uuid::parse(TEST_UUID.as_bytes()).unwrap(),
      b"2026-11-19T00:00:00.000000Z",
      Room {
        entries: 1 << 10,
        bytes: 1 << 20,
      },
    )
    .unwrap();
    let add = |path: &str, kind: NodeKind, bytes: Option<&[u8]>| ChangedPath {
      path: path.to_owned(),
      action: Action::Add,
      kind,
      copy_from: None,
      text: bytes.map(|b| Text {
        identity: *blake3::hash(b).as_bytes(),
        md5: [0; 16],
        sha1: [0; 20],
        size: u64::try_from(b.len()).unwrap(),
      }),
      properties: None,
      mergeinfo_modified: false,
    };
    let (pub_ok, secret_top, trunk_ok, trunk_unlock, tags_locked) = (
      b"pub-ok\n".as_slice(),
      b"SECRET-TOP\n".as_slice(),
      b"trunk-ok\n".as_slice(),
      b"trunk-unlock\n".as_slice(),
      b"tags-locked\n".as_slice(),
    );
    records
      .commit(slates_svn::commit::revision::Revision {
        head: 0,
        record: RevisionRecord {
          transaction: "0-1".to_owned(),
          changes: vec![
            add("/pub", NodeKind::Directory, None),
            add("/pub/ok.txt", NodeKind::File, Some(pub_ok)),
            add("/pub/secret", NodeKind::Directory, None),
            add("/pub/secret/vault.txt", NodeKind::File, Some(secret_top)),
            add("/secret", NodeKind::Directory, None),
            add("/secret/top.txt", NodeKind::File, Some(secret_top)),
            add("/trunk", NodeKind::Directory, None),
            add("/trunk/ok.txt", NodeKind::File, Some(trunk_ok)),
            add("/trunk/unlock.txt", NodeKind::File, Some(trunk_unlock)),
            add("/tags", NodeKind::Directory, None),
            add("/tags/locked.txt", NodeKind::File, Some(tags_locked)),
          ],
        },
        properties: std::collections::BTreeMap::from([
          (b"svn:author".to_vec(), b"alice".to_vec()),
          (
            b"svn:date".to_vec(),
            b"2026-11-19T00:00:01.000000Z".to_vec(),
          ),
          (b"svn:log".to_vec(), b"seed".to_vec()),
        ]),
        contents: std::collections::BTreeMap::new(),
        stored: [pub_ok, secret_top, trunk_ok, trunk_unlock, tags_locked]
          .into_iter()
          .map(|b| (*blake3::hash(b).as_bytes(), u64::try_from(b.len()).unwrap()))
          .collect(),
        lock_entries: Vec::new(),
      })
      .unwrap();
    let writer_owner = crate::lfs::owner_name(&Principal::Uid {
      host: HostId(0),
      uid: WRITER_UID,
    });
    let seeded_lock = |path: &str, token: &str| {
      slates_svn::lock::LockEntry::Set(slates_svn::lock::Lock {
        path: path.to_owned(),
        token: slates_svn::lock::LockToken::supplied(token).unwrap(),
        owner: writer_owner.clone(),
        comment: None,
        is_dav_comment: false,
        created: AprTime(1),
        expires: None,
      })
    };
    records
      .commit_record(Record::Locks(vec![
        seeded_lock("/tags/locked.txt", "opaquelocktoken:tags-lock-1"),
        seeded_lock("/trunk/unlock.txt", "opaquelocktoken:trunk-lock-1"),
      ]))
      .unwrap();
    Box::leak(Box::new(records))
  }

  fn assert_ssh_per_path_reads(cmd: &mut SvnCommand<PolicyRepo, DroppableStore>) {
    cmd
      .input(b"( get-file ( 14:secret/top.txt ( 1 ) false false ) ) ")
      .unwrap();
    let denied_file = drive_until_input(cmd);
    assert!(
      denied_file.contains("170001") && !denied_file.contains("SECRET-TOP"),
      "ssh get-file /secret/top.txt must be refused E170001: {denied_file:?}"
    );

    cmd
      .input(b"( get-dir ( 3:pub ( 1 ) false true ( kind ) ) ) ")
      .unwrap();
    let pub_dir = drive_until_input(cmd);
    assert!(
      pub_dir.contains("ok.txt") && !pub_dir.contains("secret"),
      "ssh get-dir /pub must hide /pub/secret: {pub_dir:?}"
    );
  }

  fn assert_ssh_per_path_locks(cmd: &mut SvnCommand<PolicyRepo, DroppableStore>) {
    cmd
      .input(b"( lock-many ( ( 4:mine ) false ( ( 15:tags/locked.txt ( ) ) ) ) ) ")
      .unwrap();
    let denied_lock = drive_until_input(cmd);
    assert!(
      denied_lock.contains("170001") && !denied_lock.contains("opaquelocktoken"),
      "ssh lock-many /tags/locked.txt must be refused E170001: {denied_lock:?}"
    );

    cmd
      .input(b"( lock-many ( ( 4:mine ) false ( ( 12:trunk/ok.txt ( ) ) ) ) ) ")
      .unwrap();
    let allowed_lock = drive_until_input(cmd);
    assert!(
      allowed_lock.contains("opaquelocktoken") && !allowed_lock.contains("failure"),
      "ssh lock-many /trunk/ok.txt must succeed: {allowed_lock:?}"
    );
  }

  fn assert_ssh_per_path_unlocks(cmd: &mut SvnCommand<PolicyRepo, DroppableStore>) {
    cmd
      .input(b"( unlock-many ( true ( ( 15:tags/locked.txt ( ) ) ) ) ) ")
      .unwrap();
    let denied_unlock = drive_until_input(cmd);
    assert!(
      denied_unlock.contains("170001") && !denied_unlock.contains("15:tags/locked.txt"),
      "ssh unlock-many /tags/locked.txt must be refused E170001: {denied_unlock:?}"
    );

    cmd
      .input(
        b"( unlock-many ( false ( ( 16:trunk/unlock.txt ( 28:opaquelocktoken:trunk-lock-1 ) ) ) ) ) ",
      )
      .unwrap();
    let allowed_unlock = drive_until_input(cmd);
    assert!(
      allowed_unlock.contains("( success ( 16:trunk/unlock.txt ) )")
        && !allowed_unlock.contains("failure"),
      "ssh unlock-many /trunk/unlock.txt must succeed: {allowed_unlock:?}"
    );
  }

  /// Round-4 Blockers 2 & 4 (`svn+ssh://` per-path `Deny Read` and per-path `LOCK`/`UNLOCK`):
  /// Do open an SSH tunnel under `[Allow Read / Anyone, Deny Read /secret, Deny Read /pub/secret,
  /// Allow Submit /trunk, Deny Submit /tags]` and send `get-file /secret/top.txt`, `get-dir /pub`,
  /// `lock-many /tags/locked.txt`, `lock-many /trunk/ok.txt`, `unlock-many /tags/locked.txt`, and
  /// `unlock-many /trunk/unlock.txt`. Expect: `/secret/top.txt` refused `E170001`, `/pub` hides
  /// `secret`, `/tags/locked.txt` lock and unlock refused `E170001`, and `/trunk/ok.txt` lock +
  /// `/trunk/unlock.txt` unlock succeed.
  #[test]
  fn round4_blockers2_and_4_ssh_tunnel_enforces_per_path_read_and_lock() {
    use slates_db::catalog::{
      PolicyEffect, PolicyEntry, PolicyNames, PolicyRight, PolicyRule, PolicyWho,
    };
    let user = Principal::Uid {
      host: HostId(0),
      uid: WRITER_UID,
    };
    let only_user = PolicyWho::AnyOf {
      entries: vec![PolicyEntry::Is {
        principal: user.clone(),
      }],
    };
    let path_rule =
      |pattern: &str, right: PolicyRight, effect: PolicyEffect, who: PolicyWho| PolicyRule {
        names: PolicyNames::Paths,
        pattern: pattern.to_owned(),
        right,
        effect,
        who,
      };
    let record = svn_policy_record(vec![
      path_rule(
        "/",
        PolicyRight::Read,
        PolicyEffect::Allow,
        PolicyWho::Anyone,
      ),
      path_rule(
        "/secret",
        PolicyRight::Read,
        PolicyEffect::Deny,
        only_user.clone(),
      ),
      path_rule(
        "/pub/secret",
        PolicyRight::Read,
        PolicyEffect::Deny,
        only_user.clone(),
      ),
      path_rule(
        "/trunk",
        PolicyRight::Submit,
        PolicyEffect::Allow,
        only_user.clone(),
      ),
      path_rule("/tags", PolicyRight::Submit, PolicyEffect::Deny, only_user),
    ]);
    let alive: &'static Cell<bool> = Box::leak(Box::new(Cell::new(true)));
    let records = seeded_ssh_records();
    let mut cmd = SvnCommand::new(
      PolicyRepo(record.clone()),
      DroppableStore {
        alive,
        unrecovered: false,
        records,
        record: Some(record),
      },
      &user,
      test_bounds(),
    )
    .expect("tunnel starts");

    let handshake =
      b"( 2 ( edit-pipeline svndiff1 absent-entries depth mergeinfo log-revprops ) 22:svn+ssh://fixture/demo 8:SVN/1.15 ( ) ) \
        ( EXTERNAL ( 0: ) ) ";
    cmd.input(handshake).unwrap();
    let _ = drive_until_input(&mut cmd);

    assert_ssh_per_path_reads(&mut cmd);
    assert_ssh_per_path_locks(&mut cmd);
    assert_ssh_per_path_unlocks(&mut cmd);
  }
}
