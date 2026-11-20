//! One ra_svn connection, the server's side, with no I/O (§4.17.4 "5. The wire", piece 7; A-656):
//! the greeting and authentication (A-599's [`handshake::Server`]), then every command the client
//! sends, each answered as `svnserve`'s command loop answers it (`serve.c` `serve` and
//! `svn_ra_svn__handle_commands2` over `main_commands`; Subversion's `1.14.x` at `d434dc14`, 1.15.0
//! alike):
//! - the read commands (A-613's [`reads::answer`]), a long answer's rest resumed a slice at a time
//!   within the caller's room ([`reads::Stream`]);
//! - `get-lock` and `get-locks` from [`Sources::locks`] at [`Sources::now`] (A-693 part 1,
//!   [`reads::answer_lock_read`]);
//! - `reparent`: a trivial auth request, then the session moved to the URL's path, or "'…' is not
//!   the same repository as '…'" (170000) for a URL outside the repository;
//! - `update`, `switch`, `status` and `diff`: the reporter's exchange (A-616's [`Exchange`]), its
//!   drive written within the caller's room and steps;
//! - `commit`: A-655's entry point and drive (**A commit**, below);
//! - `lock`, `lock-many`, `unlock` and `unlock-many` (A-693 part 2, [`locks`]): judged against the
//!   lock table, their entries landed by the caller through the owner's step, then answered;
//! - every other command of `svnserve`'s set, until its piece lands (`replay`, `replay-range`,
//!   `change-rev-prop`): answered as a command `svnserve`
//!   does not know, "Unknown editor command '…'" (210001), which a stock client reads as a server
//!   without it, and counted
//!   ([`Turn::Unserved`]); the connection goes on;
//! - an item that is no command ends the connection, as `svnserve` ends it.
//!
//! **Authentication for one command.** Where a command needs access the session's principal lacks,
//! the session is anonymous and the client could authenticate, `svnserve` asks for authentication
//! in the middle of the command, decides again with the principal the client proves, and keeps that
//! principal for the rest of the connection (`must_have_access`, `serve.c` 807-850). So does the
//! connection, for `commit`, which needs write access (A-655's [`Begun::Authenticate`]), for
//! `update`, which needs to read its target, and for the reads that check their path first
//! (`get-file`, `get-dir`, `check-path`, `stat`, `get-iprops`, `list`: A-613's
//! [`reads::Outcome::Authenticate`]): the request lists `EXTERNAL` and `CRAM-MD5` alone
//! ([`handshake::Server::for_command`]), the caller answers what it asks as in the handshake, and
//! once the client is established ([`Turn::Established`]) the command runs again at the next call,
//! judged by the new principal's rules with no trivial auth request before its answer, or is
//! refused "Authorization failed" (170001) where the new principal lacks the access too. A session
//! has one read rule: the rules the repository's authorization file gives its principal (A-606,
//! A-613's eighth slice), compiled at the first command that reads and again once an authentication
//! has changed the principal (`Established::compiled_read_rules`), and asked through
//! [`authz::may_read`] by the read commands, an update's target and the update's drive alike
//! (A-656 continued (2)), so a read run again after its authentication is judged by the principal
//! the client proved.
//!
//! **A commit.** The client's `commit` is handed to the caller ([`Turn::CommitAsked`]), which gives
//! the transaction's name, its instant and its room ([`Connection::commit`]). A-655's entry point
//! decides the principal's access and answers ([`commit_drive::commit`]); the client's editor
//! commands then go to its drive ([`Drive::command`]), which writes each early refusal at once, the
//! client pipelining (`edit-pipeline`). Where `svnserve` runs a hook inside a command, start-commit
//! at `open-root` and pre-commit at `close-edit` (A-661), the drive waits ([`Turn::Hook`]): the
//! caller runs it over the transaction ([`Connection::pending`]) and answers
//! ([`Connection::answer_hook`]); a failure is the command's, and the client's commands are then
//! dropped until its `abort-edit` or `success` (`editorp.c` 1149-1208). Once pre-commit passed,
//! `close-edit`'s verdict accepts the work ([`Turn::Accepted`]): the caller makes it through the
//! owner's commit step (A-655's revision of the work and the repository's commit) and answers with
//! the revision made ([`Connection::committed`]: `close-edit`'s success, the trivial auth request
//! and the new revision, `serve.c` 1555-1577 and `editorp.c` 888-905), or refuses it in the step's
//! words ([`Connection::not_committed`], [`Uncommitted::Step`]): the refusal is `close-edit`'s
//! failure, and the client's commands are then read and dropped until its `abort-edit` or
//! `success`, as `svn_ra_svn_drive_editor2` drops them (`editorp.c` 1149-1208). Until the owner's
//! step appends through the repository's logs (A-655 part 2b), the daemon refuses every work
//! ([`Uncommitted::NotServed`]); nothing else applies one.
//!
//! **One connection, two transports.** The same connection serves `svn://` over TCP and
//! `svnserve -t` through an SSH tunnel (piece 12): a tunnel's user comes in through the repository
//! its caller names for the client's URL ([`handshake::Repository::tunnel_user`]), and `EXTERNAL`
//! takes it, as `svnserve -t` takes its tunnel's.
//!
//! **What the caller does.** It hands over the client's bytes not yet taken, writes what is
//! appended, and answers what a [`Turn`] asks: resolves the client's URL ([`Connection::open`] or
//! [`Connection::refuse`]), gives a CRAM-MD5 challenge's parts ([`Connection::challenge`]) and a
//! user's key ([`Connection::key`]), begins a commit and makes or refuses its work, and from the
//! session's establishment on hands over the repository's [`Sources`] with every call, with the
//! instant its locks are judged at. What the connection waits on its client for, which the caller's
//! waits are learned by, is [`Connection::waiting`].
//!
//! **Bounds.** Every bound is the caller's: one item's bytes, the report's room and the nodes a
//! drive holds open at once ([`Limits`], each derived from its shard's budget), and a commit's room
//! ([`CommitStart`], from its admission); the deepest list a command holds is `svnserve`'s own
//! deepest prototype ([`COMMAND_DEPTH`]), so a list nested deeper than any command takes ends the
//! connection where `svnserve` would read on to 64 levels.
//!
//! No I/O, no clock, no randomness, no panic.

mod locks;
#[cfg(test)]
mod tests;

use std::ffi::CStr;

use crate::authz::{self, Authz};
use crate::commit::drive::{self as commit_drive, Begun, Drive, Due, Pending, Start, Step, Work};
use crate::commit::revision::StepRefusal;
use crate::handshake::{
  self, Access, ClientOpen, CommandMechanisms, CramKey, Principal, Repository, Session, Unserved,
  Verdict,
};
use crate::index::{Index, Revision};
use crate::lock::{AprTime, LockTable};
use crate::ra_svn::{self, Bounds, ItemError, Parsed, Parser, Value};
use crate::reads::{self, Outcome, Stream};
use crate::report::exchange::{Awaiting, Budget, Exchange, Opened, Turn as ExchangeTurn};
use crate::report::{
  self, Context, ReportFailure, Request, RequestKind, join_fspath, repository_path, uri_decode,
  write_empty_success, write_trivial_auth,
};

/// The deepest list a command holds: `lock-many`'s and `unlock-many`'s, each target a list holding
/// its revision's or token's list, inside the targets' list, inside the parameters, inside the
/// command.
/// Format: `serve.c` `lock_many`'s `"(?c)bl"` with each target `"c(?r)"`, five lists deep.
pub const COMMAND_DEPTH: usize = 5;

/// The longest word a stock parser takes.
/// Format: `marshal.c` `MAX_WORD_LENGTH`, 1.14 and 1.15 alike.
const LONGEST_WORD: usize = 24;

/// What one parsed item costs, by which an item's bytes bound its count.
/// Derived: the size of the item codec's own value, `size_of::<Value>()`.
const PARSED_ITEM_BYTES: usize = size_of::<Value<'static>>();

/// The command that moves a session to another of its repository's paths.
/// Format: `serve.c`'s `main_commands`.
const REPARENT: &str = "reparent";

/// The command that begins a commit.
/// Format: `serve.c`'s `main_commands`.
const COMMIT: &str = "commit";

/// The command that ends an edit the client gives up, which ends the drop after a refusal.
/// Format: `editorp.c` 1205.
const ABORT_EDIT: &str = "abort-edit";

/// The word of a command's success, which also ends the drop after a refusal.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_response`; `editorp.c` 1206.
const SUCCESS: &str = "success";

/// The word of a command's failure.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_failure`.
const FAILURE: &str = "failure";

/// `SVN_ERR_UNSUPPORTED_FEATURE`: a commit the owner does not make yet.
/// Format: the miscellaneous category's start, 120,000 + 16 × 5,000 = 200,000, plus 7
/// (`svn_error_codes.h`).
const SVN_ERR_UNSUPPORTED_FEATURE: u32 = 200_007;

/// The words a commit the owner does not make yet is refused with.
/// Format: slates' own, `svnserve` having no such refusal.
const NOT_SERVED: &str = "This server does not make commits yet";

/// The bounds a connection keeps, each its caller's, derived from its shard's budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
  /// The most bytes one of the client's items may take.
  pub item_bytes: usize,
  /// The most bytes a report may hold.
  pub report_room: usize,
  /// The most nodes a drive holds open at once: the deepest path the repository holds, in names,
  /// and its root.
  pub most_open: usize,
}

/// What a connection reads after the session's establishment, handed over with every call.
#[derive(Debug)]
pub struct Sources<'s, R: ?Sized> {
  /// The repository's index (A-553).
  pub index: &'s Index,
  /// The repository's green: entries, texts, property lists and revision properties, and the names
  /// it gives new content.
  pub repository: &'s R,
  /// The repository's lock table (A-602), the one record of its locks: a commit's checks, the lock
  /// verbs and an update's drive all read it (A-656 continued (3)).
  pub locks: &'s LockTable,
  /// The owner's instant at this call, at which a lock is judged expired or not.
  pub now: AprTime,
  /// Who may do what in the repository.
  pub policy: Policy<'s>,
}

/// Who may do what in a repository, as its `svnserve.conf` and authorization file say
/// (`serve.c` 571-575 and 760-850), and the name its green goes by.
#[derive(Clone, Copy, Debug)]
pub struct Policy<'s> {
  /// What an anonymous session may do (`anon-access`).
  pub anonymous: authz::Access,
  /// What an authenticated session may do (`auth-access`).
  pub authenticated: authz::Access,
  /// The authorization rules and the name they know the repository by (`authz_repos_name`); `None`
  /// where the repository names no authorization file, which `svnserve` takes as every path
  /// allowed.
  pub authz: Option<(&'s Authz, &'s [u8])>,
  /// The green's name, which FSFS's messages name where they name its directory
  /// ([`commit_drive::Setup::filesystem`]).
  pub filesystem: &'s str,
}

/// What the caller begins a commit with ([`Connection::commit`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitStart {
  /// The transaction's name (§4.17.4 item 3.1), which FSFS's messages name.
  pub transaction: String,
  /// The owner's instant, at which locks are judged expired or not.
  pub now: AprTime,
  /// The most bytes the work's new texts and property lists may hold, with the deltas being read:
  /// the caller's, from its admission.
  pub room: usize,
}

/// The revision the owner made of an accepted work ([`Connection::committed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Made<'a> {
  /// The new revision.
  pub revision: Revision,
  /// Its date, `svn:date`, as Subversion writes one.
  pub date: &'a str,
  /// Its author, `svn:author`; `None` for an anonymous commit.
  pub author: Option<&'a str>,
  /// The post-commit hook's words where it failed (A-661), which the client shows as a warning.
  pub post_commit_error: Option<&'a str>,
}

/// Why the owner did not make an accepted commit ([`Connection::not_committed`]), each written as
/// `close-edit`'s failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Uncommitted {
  /// The owner's commit step refused the revision made of the work (A-655's [`StepRefusal`]): a
  /// revision committed since the drive's verdict, "Transaction out of date" (160028), as FSFS's
  /// commit answers a transaction whose base moved; the room; corrupt records.
  Step(StepRefusal),
  /// The daemon makes no commits yet, until the owner's step appends through the repository's logs
  /// (A-655 part 2b): `SVN_ERR_UNSUPPORTED_FEATURE` (200007), "This server does not make commits
  /// yet".
  NotServed,
  /// The owner's step refused the work for a reason of its own, which no FSFS commit meets (the
  /// repository's store not held on the shard, the shard's budget, its log's append; A-698): the
  /// Subversion error and the words it is answered with.
  Refused {
    /// The error's number.
    code: u32,
    /// Its words.
    message: String,
  },
}

/// What one call of [`Connection::receive`] made of the client's bytes, and what the caller does
/// next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Turn<'a> {
  /// The client's bytes end inside its next item: at least `needed` bytes from the input's start.
  NeedMore {
    /// How many bytes from the input's start the item needs at least.
    needed: usize,
  },
  /// The first `consumed` bytes of the input were taken and answered: call again with the rest.
  Took {
    /// The bytes taken.
    consumed: usize,
  },
  /// Output was written and none of the input taken (a drive's next steps, an answer's next slice,
  /// a command run again after its authentication): write the output, then call again with the
  /// same input.
  Wrote,
  /// The client answered the greeting, `consumed` bytes: the caller resolves its URL and calls
  /// [`Connection::open`] or [`Connection::refuse`].
  Opened {
    /// The client's answer.
    open: ClientOpen<'a>,
    /// The bytes taken.
    consumed: usize,
  },
  /// The client chose CRAM-MD5: the caller gives the challenge's parts
  /// ([`Connection::challenge`]) or says it has none ([`Connection::challenge_failed`]).
  ChallengeNeeded {
    /// The bytes taken.
    consumed: usize,
  },
  /// The client's CRAM-MD5 response named `user`: the caller gives that user's key
  /// ([`Connection::key`]).
  KeyNeeded {
    /// The user, as the response named it.
    user: &'a [u8],
    /// The bytes taken.
    consumed: usize,
  },
  /// The client authenticated as `principal`, at the handshake's end or for one command: from here
  /// on every call hands over the repository's sources. After an authentication for one command,
  /// the next call runs the command again.
  Established {
    /// Who.
    principal: Principal,
    /// The bytes taken.
    consumed: usize,
  },
  /// The client asked for a commit, `consumed` bytes: the caller gives what it begins with
  /// ([`Connection::commit`]).
  CommitAsked {
    /// The bytes taken.
    consumed: usize,
  },
  /// The commit's drive waits for a hook `svnserve` runs inside the command it just took,
  /// `consumed` bytes (A-661): start-commit at `open-root`, pre-commit at `close-edit`. The caller
  /// runs it over the transaction ([`Connection::pending`]) and answers
  /// ([`Connection::answer_hook`]) before anything else.
  Hook {
    /// The hook.
    due: Due,
    /// The bytes taken.
    consumed: usize,
  },
  /// The commit's drive accepted its work at `close-edit`: the caller makes the work, then answers
  /// with the revision it made ([`Connection::committed`]) or refuses it
  /// ([`Connection::not_committed`]). `consumed` is 0 where the acceptance came of a hook's answer.
  Accepted {
    /// The work.
    work: Box<Work>,
    /// The bytes taken.
    consumed: usize,
  },
  /// The client asked for a lock verb, `consumed` bytes (A-693 part 2): the caller judges it
  /// ([`Connection::lock_verb`]), lands its entries, and reports their landing
  /// ([`Connection::locks_landed`]).
  LockAsked {
    /// The bytes taken.
    consumed: usize,
  },
  /// The client asked for a command of `svnserve`'s set slates does not serve yet: it was answered
  /// "Unknown editor command" (210001), and the connection goes on.
  Unserved {
    /// The command's name.
    command: &'a str,
    /// The bytes taken.
    consumed: usize,
  },
}

/// What the connection waits on its client for, by which its caller learns each wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waiting {
  /// The handshake's answers, or an authentication's for one command.
  Handshake,
  /// A command, or the rest of one begun.
  Command,
  /// A report's commands.
  Report,
  /// Nothing from the client: an answer is being written (a drive, a long read's rest, a command
  /// run again), or the caller's own answer is awaited (a commit's start, a hook's answer, its
  /// work's verdict).
  Writing,
  /// The client's answer to `close-edit` or `abort-edit`.
  EditAnswer,
  /// A commit's editor commands, or the client's `abort-edit` after its refusal.
  Edit,
  /// Nothing more: the connection has ended.
  Ended,
}

/// Why the connection ends: nothing more is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
  /// The handshake's refusal.
  Handshake(handshake::Refused),
  /// The report's refusal, or the drive's writer's.
  Report(report::Refused),
  /// The client's bytes are no item within the bounds (the codec's refusal).
  Item(ItemError),
  /// An item that is no command, or a command's parameters `svnserve` refuses: "Malformed network
  /// data" (210004), on which `svnserve` ends the connection without a word.
  Malformed,
  /// Where `svnserve`'s process aborts on an assertion that fails: `get-file-revs` of a file whose
  /// first location the session may not read (A-613's eighth slice). It sets no malfunction
  /// handler, so Subversion's default aborts and the connection ends without a further word.
  Aborted,
  /// A call the connection does not take where it stands: the caller's error, not the client's.
  OutOfTurn,
  /// A call after the session's establishment without the repository's sources: the caller's error.
  NoSources,
  /// A repository whose root URL is no ancestor of the client's URL, or names no UTF-8 path once
  /// decoded: the caller's error.
  RootUrl,
}

/// How the client may authenticate, kept from the repository the caller named for the session's
/// URL for an authentication for one command.
#[derive(Clone, Debug)]
struct Mechanisms {
  /// The authentication realm.
  realm: Vec<u8>,
  /// The tunnel's user, where authenticated users may read (`EXTERNAL`).
  tunnel_user: Option<Vec<u8>>,
  /// Whether authenticated users may read and the repository holds their keys (`CRAM-MD5`).
  cram_md5: bool,
}

/// What the caller's answer to the client's URL settled, kept until the session is established.
#[derive(Clone, Debug)]
struct Resolved {
  /// The repository path the URL names.
  base: String,
  /// The repository's root URL, decoded.
  repository_url: String,
  /// How the client may authenticate.
  mechanisms: Mechanisms,
}

/// The established session, with the path its URL names now.
#[derive(Clone, Debug)]
struct Established {
  /// The session.
  session: Session,
  /// The repository path the session's URL names (`svnserve`'s `fs_path`), moved by `reparent`.
  base: String,
  /// The repository's UUID.
  uuid: String,
  /// The repository's root URL, decoded as `svn_path_uri_decode` decodes it, which a commit's copy
  /// sources must begin with (`serve.c` 1551).
  repository_url: String,
  /// How the client may authenticate for one command.
  mechanisms: Mechanisms,
  /// The rules the session's reads are judged by, with the principal they were compiled for; `None`
  /// until a read needs them ([`Established::reads_session`]).
  read_rules: Option<ReadRules>,
}

/// A session's read rules and the principal they were compiled for (A-606): `svnserve` keeps one
/// compiled set per user and repository, so a principal's rules are compiled at the first read that
/// needs them and again only once an authentication has changed the principal.
#[derive(Clone, Debug)]
struct ReadRules {
  /// Whom they were compiled for.
  principal: Principal,
  /// The repository's rules for that principal; `None` where it names no authorization file, and
  /// then every path is readable.
  rules: Option<authz::PrincipalRules>,
}

impl ReadRules {
  /// The rules `policy`'s authorization file gives `principal`, compiled into `held` at the first
  /// call and again only where `held` was compiled for another principal, an authentication having
  /// changed it since; none where the repository names no authorization file.
  fn for_principal<'held>(
    held: &'held mut Option<Self>,
    principal: &Principal,
    policy: &Policy<'_>,
  ) -> Option<&'held authz::PrincipalRules> {
    if held
      .as_ref()
      .is_some_and(|compiled| compiled.principal != *principal)
    {
      *held = None;
    }
    let compiled = held.get_or_insert_with(|| {
      let user = authz::Principal::named(user_of(principal));
      Self {
        principal: principal.clone(),
        rules: policy
          .authz
          .map(|(rules_file, name)| rules_file.rules(name, user)),
      }
    });
    compiled.rules.as_ref()
  }
}

impl Established {
  /// The session's one read rule ([`ReadRules::for_principal`]), which the read commands, an
  /// update's target and the update's drive all judge paths by ([`authz::may_read`]).
  fn compiled_read_rules(&mut self, policy: &Policy<'_>) -> Option<&authz::PrincipalRules> {
    ReadRules::for_principal(&mut self.read_rules, &self.session.principal, policy)
  }

  /// How the client may authenticate for one command needing `level`, as `must_have_access` finds
  /// it (`serve.c` 829-834, `send_mechs` with a username needed): `None` where the session already
  /// has a user, where authenticated users lack `level`, or where the session kept no way in;
  /// else `EXTERNAL` for the tunnel's user and `CRAM-MD5` where the repository holds keys, never
  /// `ANONYMOUS`.
  fn mechanisms_for(
    &self,
    policy: &Policy<'_>,
    level: authz::Access,
  ) -> Option<CommandMechanisms<'_>> {
    if self.session.principal != Principal::Anonymous {
      return None;
    }
    let allowed = policy.authenticated >= level;
    let kept = &self.mechanisms;
    let mechanisms = CommandMechanisms {
      realm: &kept.realm,
      tunnel_user: kept.tunnel_user.as_deref().filter(|_| allowed),
      cram_md5: kept.cram_md5 && allowed,
    };
    (mechanisms.tunnel_user.is_some() || mechanisms.cram_md5).then_some(mechanisms)
  }

  /// Where the session's authentication stands for a read's path check: the client could
  /// authenticate for read access ([`Established::mechanisms_for`]), or nothing can be asked.
  fn reads_authentication(&self, policy: &Policy<'_>) -> reads::Authentication {
    if self.mechanisms_for(policy, authz::Access::Read).is_some() {
      reads::Authentication::Available
    } else {
      reads::Authentication::Settled
    }
  }

  /// The session the read commands run in: the path the session's URL names, the repository's
  /// UUID, what the client takes, the session's one read rule, and where its `authentication`
  /// stands.
  fn reads_session(
    &mut self,
    policy: &Policy<'_>,
    authentication: reads::Authentication,
  ) -> reads::Session<'_> {
    let authz = ReadRules::for_principal(&mut self.read_rules, &self.session.principal, policy);
    reads::Session {
      base: &self.base,
      uuid: &self.uuid,
      capabilities: self.session.capabilities,
      authz,
      authentication,
    }
  }
}

/// The user `principal` names, as an authorization file's rules match it: none for an anonymous
/// one.
fn user_of(principal: &Principal) -> Option<&[u8]> {
  match principal {
    Principal::Anonymous => None,
    Principal::User(name) => Some(name.as_slice()),
  }
}

/// A command an authentication for it interrupted, run again once the client is established.
#[derive(Debug)]
enum Interrupted {
  /// `commit`: its item, and what the caller began it with.
  Commit {
    /// The item.
    command: Vec<u8>,
    /// The caller's start.
    start: CommitStart,
  },
  /// `update`: its item.
  Update {
    /// The item.
    command: Vec<u8>,
  },
  /// A read that checks its path first (`get-file`, `get-dir`, `check-path`, `stat`, `get-iprops`,
  /// `list`): its item.
  Read {
    /// The item.
    command: Vec<u8>,
  },
  /// A lock verb (A-693 part 2): its item.
  Lock {
    /// The item.
    command: Vec<u8>,
  },
}

/// Where the connection stands.
enum Phase {
  /// The handshake runs.
  Handshake(Box<handshake::Server>),
  /// Between commands.
  Commands(Box<Established>),
  /// A read's answer has more to write.
  Streaming(Box<Established>, Box<Stream>),
  /// A report command's exchange runs.
  Reporting(Box<Established>, Box<Exchange>),
  /// `commit` was read, its item kept: the caller's start is awaited.
  CommitAsked(Box<Established>, Vec<u8>),
  /// An authentication for one command runs.
  Authenticating(Box<Established>, Box<handshake::Server>, Box<Interrupted>),
  /// An authentication for one command ended: the command runs again at the next call.
  Resuming(Box<Established>, Box<Interrupted>),
  /// A commit's drive takes the client's editor commands.
  Committing(Box<Established>, Box<Drive>),
  /// A commit's drive waits for a hook's answer, the caller's.
  Hooked(Box<Established>, Box<Drive>),
  /// A commit's work was accepted: the caller's verdict is awaited.
  Accepted(Box<Established>),
  /// A commit's work was refused at `close-edit`: the client's commands are dropped until its
  /// `abort-edit` or `success`.
  Dropping(Box<Established>),
  /// A lock verb was read, its item kept, and whether the client has just authenticated for it: the
  /// caller's judging is awaited ([`Connection::lock_verb`]).
  LockAsked(Box<Established>, Vec<u8>, bool),
  /// A lock verb was judged, its entries with the caller: their landing is awaited
  /// ([`Connection::locks_landed`]).
  Locking(Box<Established>, Box<locks::Pending>),
  /// The connection has ended.
  Ended,
}

impl std::fmt::Debug for Phase {
  /// The phase's name: a drive holds the work being read, which is no one's to print.
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(match self {
      Self::Handshake(_) => "Handshake",
      Self::Commands(_) => "Commands",
      Self::Streaming(..) => "Streaming",
      Self::Reporting(..) => "Reporting",
      Self::CommitAsked(..) => "CommitAsked",
      Self::Authenticating(..) => "Authenticating",
      Self::Resuming(..) => "Resuming",
      Self::Committing(..) => "Committing",
      Self::Hooked(..) => "Hooked",
      Self::Accepted(_) => "Accepted",
      Self::Dropping(_) => "Dropping",
      Self::LockAsked(..) => "LockAsked",
      Self::Locking(..) => "Locking",
      Self::Ended => "Ended",
    })
  }
}

/// One connection's server side (module documentation).
#[derive(Debug)]
pub struct Connection {
  /// Where it stands.
  phase: Phase,
  /// Its bounds.
  limits: Limits,
  /// The client's URL, from its answer to the greeting until the caller resolves it.
  url: Vec<u8>,
  /// What the caller's answer to the URL settled, until the session is established.
  resolved: Option<Resolved>,
}

impl Connection {
  /// A connection whose greeting is appended to `out`.
  ///
  /// # Errors
  /// None in practice: the greeting's words are the grammar's.
  pub fn new(limits: Limits, out: &mut Vec<u8>) -> Result<Self, Refused> {
    let server = handshake::Server::new(limits.item_bytes, out).map_err(Refused::Handshake)?;
    Ok(Self {
      phase: Phase::Handshake(Box::new(server)),
      limits,
      url: Vec::new(),
      resolved: None,
    })
  }

  /// What the connection waits on its client for now.
  pub fn waiting(&self) -> Waiting {
    match &self.phase {
      Phase::Handshake(_) | Phase::Authenticating(..) => Waiting::Handshake,
      Phase::Commands(_) => Waiting::Command,
      Phase::Streaming(..)
      | Phase::CommitAsked(..)
      | Phase::Resuming(..)
      | Phase::Hooked(..)
      | Phase::Accepted(_)
      | Phase::LockAsked(..)
      | Phase::Locking(..) => Waiting::Writing,
      Phase::Reporting(_, exchange) => match exchange.awaiting() {
        Awaiting::Report => Waiting::Report,
        Awaiting::Drive => Waiting::Writing,
        Awaiting::Answer => Waiting::EditAnswer,
        Awaiting::Nothing => Waiting::Command,
      },
      Phase::Committing(..) | Phase::Dropping(_) => Waiting::Edit,
      Phase::Ended => Waiting::Ended,
    }
  }

  /// The established session, once the client has authenticated.
  pub fn session(&self) -> Option<&Session> {
    match &self.phase {
      Phase::Commands(established)
      | Phase::Streaming(established, _)
      | Phase::Reporting(established, _)
      | Phase::CommitAsked(established, _)
      | Phase::Authenticating(established, _, _)
      | Phase::Resuming(established, _)
      | Phase::Committing(established, _)
      | Phase::Hooked(established, _)
      | Phase::Accepted(established)
      | Phase::Dropping(established)
      | Phase::LockAsked(established, _, _)
      | Phase::Locking(established, _) => Some(&established.session),
      Phase::Handshake(_) | Phase::Ended => None,
    }
  }

  /// Goes on with `input`, the client's bytes not yet taken, appending to `out` within `budget`;
  /// from the session's establishment on, with the repository's `sources`.
  ///
  /// # Errors
  /// [`Refused`] for client bytes that end the connection, and for the caller's own errors: a call
  /// out of turn (where a commit's start, a hook's answer or its work's verdict is awaited, or
  /// after the end), or one without sources, which ends the connection.
  pub fn receive<'a, R>(
    &mut self,
    sources: Option<&Sources<'_, R>>,
    input: &'a [u8],
    out: &mut Vec<u8>,
    budget: Budget,
  ) -> Result<Turn<'a>, Refused>
  where
    R: reads::Repository + commit_drive::Green + ?Sized,
  {
    if matches!(
      self.phase,
      Phase::CommitAsked(..)
        | Phase::Hooked(..)
        | Phase::Accepted(_)
        | Phase::LockAsked(..)
        | Phase::Locking(..)
        | Phase::Ended
    ) {
      return Err(Refused::OutOfTurn);
    }
    match std::mem::replace(&mut self.phase, Phase::Ended) {
      Phase::Handshake(server) => self.handshake(server, input, out),
      Phase::Authenticating(established, server, interrupted) => {
        self.authenticating(established, server, interrupted, input, out)
      }
      phase => {
        let sources = sources.ok_or(Refused::NoSources)?;
        self.established(phase, sources, input, out, budget)
      }
    }
  }

  /// Answers the client's URL with `repository`, which names the root of the repository holding it
  /// (`svnserve`'s `find_repos`): the authentication request, or "No access allowed to this
  /// repository" and the connection's end.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless the client's answer was just read; [`Refused::RootUrl`] for a
  /// root that is no ancestor of the client's URL, or names no UTF-8 path once decoded.
  pub fn open(
    &mut self,
    repository: &Repository<'_>,
    out: &mut Vec<u8>,
  ) -> Result<Access, Refused> {
    let Phase::Handshake(server) = &mut self.phase else {
      return Err(Refused::OutOfTurn);
    };
    let url = std::str::from_utf8(&self.url).map_err(|_| Refused::RootUrl)?;
    let base = repository_path(repository.root_url, url).map_err(|_| Refused::RootUrl)?;
    let repository_url = decoded_url(repository.root_url).ok_or(Refused::RootUrl)?;
    let access = server.open(repository, out).map_err(Refused::Handshake)?;
    match access {
      Access::Asked => {
        self.resolved = Some(Resolved {
          base,
          repository_url,
          mechanisms: Mechanisms {
            realm: repository.realm.to_vec(),
            tunnel_user: repository.tunnel_user.map(<[u8]>::to_vec),
            cram_md5: repository.cram_md5,
          },
        });
      }
      Access::Denied => self.phase = Phase::Ended,
    }
    Ok(access)
  }

  /// Answers the client's URL with the command failure `unserved`; the connection ends.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless the client's answer was just read.
  pub fn refuse(&mut self, unserved: Unserved<'_>, out: &mut Vec<u8>) -> Result<(), Refused> {
    let Phase::Handshake(server) = &mut self.phase else {
      return Err(Refused::OutOfTurn);
    };
    server.refuse(unserved, out).map_err(Refused::Handshake)?;
    self.phase = Phase::Ended;
    Ok(())
  }

  /// Sends the CRAM-MD5 challenge `<nonce.time@host>`, its parts the caller's.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless the client just chose CRAM-MD5.
  pub fn challenge(
    &mut self,
    nonce: u64,
    time_micros: i64,
    host: &CStr,
    out: &mut Vec<u8>,
  ) -> Result<(), Refused> {
    self
      .authenticator()?
      .challenge(nonce, time_micros, host, out)
      .map_err(Refused::Handshake)
  }

  /// Fails the CRAM-MD5 attempt the client just chose: no challenge could be made.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless the client just chose CRAM-MD5.
  pub fn challenge_failed(&mut self, out: &mut Vec<u8>) -> Result<(), Refused> {
    self
      .authenticator()?
      .challenge_failed(out)
      .map_err(Refused::Handshake)
  }

  /// Verifies the CRAM-MD5 response with the named user's key, `None` where the repository holds
  /// none: established, the session's commands follow, or the command the authentication was for
  /// runs again at the next call.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless a response naming a user was just read.
  pub fn key(&mut self, key: Option<&CramKey>, out: &mut Vec<u8>) -> Result<Verdict, Refused> {
    match std::mem::replace(&mut self.phase, Phase::Ended) {
      Phase::Handshake(mut server) => match server.key(key, out) {
        Ok(Verdict::Established(principal)) => {
          self.establish(&server)?;
          Ok(Verdict::Established(principal))
        }
        other => {
          self.phase = Phase::Handshake(server);
          other.map_err(Refused::Handshake)
        }
      },
      Phase::Authenticating(established, mut server, interrupted) => match server.key(key, out) {
        Ok(Verdict::Established(principal)) => {
          self.authenticated(established, &server, interrupted)?;
          Ok(Verdict::Established(principal))
        }
        other => {
          self.phase = Phase::Authenticating(established, server, interrupted);
          other.map_err(Refused::Handshake)
        }
      },
      other => {
        self.phase = other;
        Err(Refused::OutOfTurn)
      }
    }
  }

  /// Begins the commit the client asked for ([`Turn::CommitAsked`]) with `start`, through A-655's
  /// entry point as `serve.c` `commit` begins it: its answer appended to `out`, after which the
  /// drive takes the client's editor commands; or an authentication for write asked first, the
  /// session anonymous and the client able to authenticate; or the commit refused and over.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless the client just asked for a commit; [`Refused::Malformed`] for
  /// parameters `svnserve` cannot read, which end the connection.
  pub fn commit<R>(
    &mut self,
    start: CommitStart,
    sources: &Sources<'_, R>,
    out: &mut Vec<u8>,
  ) -> Result<(), Refused>
  where
    R: ?Sized,
  {
    let (established, command) = match std::mem::replace(&mut self.phase, Phase::Ended) {
      Phase::CommitAsked(established, command) => (established, command),
      other => {
        self.phase = other;
        return Err(Refused::OutOfTurn);
      }
    };
    self.begin_commit(established, command, start, false, &sources.policy, out)
  }

  /// The transaction a due hook runs over (A-661): its name, base, revision properties, rows and
  /// contents so far, as the drive holds them; none unless a hook's answer is awaited.
  pub fn pending(&self) -> Option<Pending<'_>> {
    match &self.phase {
      Phase::Hooked(_, drive) => Some(drive.pending()),
      _ => None,
    }
  }

  /// Answers the hook the drive waits for: `Ok` lets the command go on; `Err` holds the hook's
  /// failure, outermost first, written as the command's failure, after which the client's commands
  /// are dropped until its `abort-edit` or `success` (A-655's [`Drive::resume`]). The turn is the
  /// drive's next step's, none of the client's input taken: [`Turn::Took`] with nothing consumed,
  /// or [`Turn::Accepted`] where pre-commit's answer let `close-edit`'s verdict accept the work.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless a hook's answer is awaited; [`Refused::Item`] where the failure
  /// cannot be written.
  pub fn answer_hook<R>(
    &mut self,
    answer: Result<(), Vec<(u32, String)>>,
    sources: &Sources<'_, R>,
    out: &mut Vec<u8>,
  ) -> Result<Turn<'static>, Refused>
  where
    R: commit_drive::Green + ?Sized,
  {
    let (established, mut drive) = match std::mem::replace(&mut self.phase, Phase::Ended) {
      Phase::Hooked(established, drive) => (established, drive),
      other => {
        self.phase = other;
        return Err(Refused::OutOfTurn);
      }
    };
    let step = drive
      .resume(
        sources.index,
        sources.repository,
        sources.locks,
        answer,
        out,
      )
      .map_err(Refused::Item)?;
    self.stepped(established, drive, step, 0)
  }

  /// Answers the accepted commit with the revision the owner made of it (`serve.c` 1555-1577,
  /// [`commit_drive::write_commit_info`]): `close-edit`'s success, the trivial auth request, then
  /// the revision, its date, its author and the post-commit hook's words; the session's commands
  /// follow.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless a work was just accepted.
  pub fn committed(&mut self, made: &Made<'_>, out: &mut Vec<u8>) -> Result<(), Refused> {
    let established = match std::mem::replace(&mut self.phase, Phase::Ended) {
      Phase::Accepted(established) => established,
      other => {
        self.phase = other;
        return Err(Refused::OutOfTurn);
      }
    };
    commit_drive::write_commit_info(
      made.revision,
      made.date,
      made.author,
      made.post_commit_error,
      out,
    )
    .map_err(Refused::Item)?;
    self.phase = Phase::Commands(established);
    Ok(())
  }

  /// Refuses the accepted commit with `failure`, written as `close-edit`'s failure
  /// (`editorp.c` 1149-1174); the client's commands are then read and dropped until its
  /// `abort-edit` or `success` (1193-1208), and the session's commands follow.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless a work was just accepted.
  pub fn not_committed(&mut self, failure: &Uncommitted, out: &mut Vec<u8>) -> Result<(), Refused> {
    let established = match std::mem::replace(&mut self.phase, Phase::Ended) {
      Phase::Accepted(established) => established,
      other => {
        self.phase = other;
        return Err(Refused::OutOfTurn);
      }
    };
    let errors = match failure {
      Uncommitted::Step(refusal) => vec![(refusal.subversion_code(), refusal.message())],
      Uncommitted::NotServed => vec![(SVN_ERR_UNSUPPORTED_FEATURE, NOT_SERVED.to_owned())],
      Uncommitted::Refused { code, message } => vec![(*code, message.clone())],
    };
    write_failure(&errors, out)?;
    self.phase = Phase::Dropping(established);
    Ok(())
  }

  /// The handshake or the authentication for one command, whichever runs: the CRAM-MD5 calls'
  /// server.
  fn authenticator(&mut self) -> Result<&mut handshake::Server, Refused> {
    match &mut self.phase {
      Phase::Handshake(server) | Phase::Authenticating(_, server, _) => Ok(server.as_mut()),
      _ => Err(Refused::OutOfTurn),
    }
  }

  /// A phase of the established session, on, with the repository's sources.
  fn established<'a, R>(
    &mut self,
    phase: Phase,
    sources: &Sources<'_, R>,
    input: &'a [u8],
    out: &mut Vec<u8>,
    budget: Budget,
  ) -> Result<Turn<'a>, Refused>
  where
    R: reads::Repository + commit_drive::Green + ?Sized,
  {
    match phase {
      Phase::Commands(established) => self.command(established, sources, input, out),
      Phase::Streaming(established, stream) => {
        self.stream(established, stream, sources, out, budget)
      }
      Phase::Reporting(established, exchange) => {
        self.exchange(established, exchange, sources, input, out, budget)
      }
      Phase::Resuming(established, interrupted) => {
        self.resume(established, *interrupted, sources, out)
      }
      Phase::Committing(established, drive) => {
        self.editing(established, drive, sources, input, out)
      }
      Phase::Dropping(established) => self.dropping(established, input),
      Phase::Handshake(_)
      | Phase::Authenticating(..)
      | Phase::CommitAsked(..)
      | Phase::Hooked(..)
      | Phase::Accepted(_)
      | Phase::LockAsked(..)
      | Phase::Locking(..)
      | Phase::Ended => Err(Refused::OutOfTurn),
    }
  }

  /// The handshake's next step.
  fn handshake<'a>(
    &mut self,
    mut server: Box<handshake::Server>,
    input: &'a [u8],
    out: &mut Vec<u8>,
  ) -> Result<Turn<'a>, Refused> {
    let turn = match server.receive(input, out).map_err(Refused::Handshake)? {
      handshake::Progress::Established {
        principal,
        consumed,
      } => {
        self.establish(&server)?;
        return Ok(Turn::Established {
          principal,
          consumed,
        });
      }
      handshake::Progress::NeedMore { needed } => Turn::NeedMore { needed },
      handshake::Progress::Opened { open, consumed } => {
        self.url = open.url.to_vec();
        Turn::Opened { open, consumed }
      }
      handshake::Progress::ChallengeNeeded { consumed } => Turn::ChallengeNeeded { consumed },
      handshake::Progress::KeyNeeded { user, consumed } => Turn::KeyNeeded { user, consumed },
      handshake::Progress::Failed { consumed, .. } => Turn::Took { consumed },
    };
    self.phase = Phase::Handshake(server);
    Ok(turn)
  }

  /// The session established: its commands follow at the path the client's URL named.
  fn establish(&mut self, server: &handshake::Server) -> Result<(), Refused> {
    let session = server.session().cloned().ok_or(Refused::OutOfTurn)?;
    let resolved = self.resolved.take().ok_or(Refused::OutOfTurn)?;
    let uuid = String::from_utf8_lossy(&session.uuid).into_owned();
    self.phase = Phase::Commands(Box::new(Established {
      session,
      base: resolved.base,
      uuid,
      repository_url: resolved.repository_url,
      mechanisms: resolved.mechanisms,
      read_rules: None,
    }));
    Ok(())
  }

  /// An authentication for one command's next step.
  fn authenticating<'a>(
    &mut self,
    established: Box<Established>,
    mut server: Box<handshake::Server>,
    interrupted: Box<Interrupted>,
    input: &'a [u8],
    out: &mut Vec<u8>,
  ) -> Result<Turn<'a>, Refused> {
    let turn = match server.receive(input, out).map_err(Refused::Handshake)? {
      handshake::Progress::Established {
        principal,
        consumed,
      } => {
        self.authenticated(established, &server, interrupted)?;
        return Ok(Turn::Established {
          principal,
          consumed,
        });
      }
      handshake::Progress::NeedMore { needed } => Turn::NeedMore { needed },
      handshake::Progress::ChallengeNeeded { consumed } => Turn::ChallengeNeeded { consumed },
      handshake::Progress::KeyNeeded { user, consumed } => Turn::KeyNeeded { user, consumed },
      handshake::Progress::Failed { consumed, .. } => Turn::Took { consumed },
      handshake::Progress::Opened { .. } => return Err(Refused::OutOfTurn),
    };
    self.phase = Phase::Authenticating(established, server, interrupted);
    Ok(turn)
  }

  /// An authentication for one command ended with the client established: the session goes on as
  /// the principal it proved, and the command runs again at the next call.
  fn authenticated(
    &mut self,
    mut established: Box<Established>,
    server: &handshake::Server,
    interrupted: Box<Interrupted>,
  ) -> Result<(), Refused> {
    established.session = server.session().cloned().ok_or(Refused::OutOfTurn)?;
    self.phase = Phase::Resuming(established, interrupted);
    Ok(())
  }

  /// The client's next command, answered.
  fn command<'a, R>(
    &mut self,
    mut established: Box<Established>,
    sources: &Sources<'_, R>,
    input: &'a [u8],
    out: &mut Vec<u8>,
  ) -> Result<Turn<'a>, Refused>
  where
    R: reads::Repository + ?Sized,
  {
    let (value, consumed) = match self.parse(input)? {
      Parsed::NeedMore { needed } => {
        self.phase = Phase::Commands(established);
        return Ok(Turn::NeedMore { needed });
      }
      Parsed::Item { value, consumed } => (value, consumed),
    };
    let authentication = established.reads_authentication(&sources.policy);
    let session = established.reads_session(&sources.policy, authentication);
    let outcome = read_answer(sources, &session, &value, out).map_err(Refused::Item)?;
    match outcome {
      Outcome::Answered => {
        self.phase = Phase::Commands(established);
        Ok(Turn::Took { consumed })
      }
      Outcome::Authenticate => {
        let item = input.get(..consumed).ok_or(Refused::Malformed)?;
        self.ask_for_read(established, item.to_vec(), &sources.policy, out)?;
        Ok(Turn::Took { consumed })
      }
      Outcome::Streaming(stream) => {
        self.phase = Phase::Streaming(established, Box::new(stream));
        Ok(Turn::Took { consumed })
      }
      Outcome::Close => Err(Refused::Malformed),
      Outcome::Elsewhere => {
        let (name, params) = command_of(&value).ok_or(Refused::Malformed)?;
        let item = input.get(..consumed).ok_or(Refused::Malformed)?;
        let answered = self.elsewhere(&mut established, sources, name, params, item, out)?;
        Ok(match answered {
          Answered::Here => {
            self.phase = Phase::Commands(established);
            Turn::Took { consumed }
          }
          Answered::Reporting(exchange) => {
            self.phase = Phase::Reporting(established, exchange);
            Turn::Took { consumed }
          }
          Answered::Authenticating(server, interrupted) => {
            self.phase = Phase::Authenticating(established, server, interrupted);
            Turn::Took { consumed }
          }
          Answered::CommitAsked => {
            self.phase = Phase::CommitAsked(established, item.to_vec());
            Turn::CommitAsked { consumed }
          }
          Answered::LockAsked => {
            self.phase = Phase::LockAsked(established, item.to_vec(), false);
            Turn::LockAsked { consumed }
          }
          Answered::Unserved => {
            self.phase = Phase::Commands(established);
            Turn::Unserved {
              command: name,
              consumed,
            }
          }
        })
      }
    }
  }

  /// A command the read commands hand over, whose item is `item`: `reparent`, `commit`, a report
  /// command, or one not served yet.
  fn elsewhere<R>(
    &self,
    established: &mut Established,
    sources: &Sources<'_, R>,
    name: &str,
    params: &[Value<'_>],
    item: &[u8],
    out: &mut Vec<u8>,
  ) -> Result<Answered, Refused>
  where
    R: ?Sized,
  {
    if name == REPARENT {
      reparent(established, params, out)?;
      return Ok(Answered::Here);
    }
    if name == COMMIT {
      return Ok(Answered::CommitAsked);
    }
    if locks::is_lock_verb(name) {
      return Ok(Answered::LockAsked);
    }
    let Some(kind) = RequestKind::of(name) else {
      ReportFailure::UnknownCommand {
        name: name.to_owned(),
      }
      .write(out)
      .map_err(Refused::Report)?;
      return Ok(Answered::Unserved);
    };
    let request = Request::read(kind, params).map_err(Refused::Report)?;
    let target = join_fspath(&established.base, &request.target);
    let readable = authz::may_read(established.compiled_read_rules(&sources.policy), &target);
    if kind == RequestKind::Update
      && !readable
      && let Some(server) = self.ask(established, &sources.policy, authz::Access::Read, out)?
    {
      let interrupted = Interrupted::Update {
        command: item.to_vec(),
      };
      return Ok(Answered::Authenticating(server, Box::new(interrupted)));
    }
    Ok(
      match self.begin_report(established, sources.index, request, readable, false, out)? {
        Some(exchange) => Answered::Reporting(exchange),
        None => Answered::Here,
      },
    )
  }

  /// An authentication for one command needing `level`, as `must_have_access` asks for it
  /// (`serve.c` 832-836): where the session is anonymous and the client could authenticate to
  /// `level`, its request appended to `out`; otherwise none, nothing written.
  fn ask(
    &self,
    established: &Established,
    policy: &Policy<'_>,
    level: authz::Access,
    out: &mut Vec<u8>,
  ) -> Result<Option<Box<handshake::Server>>, Refused> {
    let Some(mechanisms) = established.mechanisms_for(policy, level) else {
      return Ok(None);
    };
    let server = handshake::Server::for_command(
      established.session.clone(),
      &mechanisms,
      self.limits.item_bytes,
      out,
    )
    .map_err(Refused::Handshake)?;
    Ok(Some(Box::new(server)))
  }

  /// A read the anonymous session may not make where the client could authenticate
  /// (`must_have_access` 829-834): the request for an authentication for it, the read kept to run
  /// again once the client is established. Where no way in is offered after all, the read is
  /// refused as one no authentication can help, "Authorization failed" (170001) with nothing
  /// before it.
  fn ask_for_read(
    &mut self,
    established: Box<Established>,
    command: Vec<u8>,
    policy: &Policy<'_>,
    out: &mut Vec<u8>,
  ) -> Result<(), Refused> {
    match self.ask(&established, policy, authz::Access::Read, out)? {
      Some(server) => {
        let interrupted = Interrupted::Read { command };
        self.phase = Phase::Authenticating(established, server, Box::new(interrupted));
      }
      None => {
        ReportFailure::NotAuthorized
          .write(out)
          .map_err(Refused::Report)?;
        self.phase = Phase::Commands(established);
      }
    }
    Ok(())
  }

  /// A read an authentication interrupted, run again with the principal the client proved and no
  /// trivial auth request before its answer (`must_have_access` after its `auth_request`): between
  /// commands after it, or streaming its answer.
  fn read_again<R>(
    &mut self,
    mut established: Box<Established>,
    command: &[u8],
    sources: &Sources<'_, R>,
    out: &mut Vec<u8>,
  ) -> Result<(), Refused>
  where
    R: reads::Repository + ?Sized,
  {
    let value = self.whole_item(command)?;
    let session = established.reads_session(&sources.policy, reads::Authentication::Done);
    let outcome = read_answer(sources, &session, &value, out).map_err(Refused::Item)?;
    self.phase = match outcome {
      Outcome::Answered => Phase::Commands(established),
      Outcome::Streaming(stream) => Phase::Streaming(established, Box::new(stream)),
      // The read answered `Authenticate` once and is read whole again: with `Done` it asks for
      // nothing and is the same read, so none of these comes back; were one to, the connection
      // ends rather than leave the command unanswered.
      Outcome::Close | Outcome::Elsewhere | Outcome::Authenticate => {
        return Err(Refused::Malformed);
      }
    };
    Ok(())
  }

  /// A report command's exchange begun (`Reporter::begin`): its exchange, or `None` where it was
  /// refused at once, its refusal written.
  fn begin_report(
    &self,
    established: &Established,
    index: &Index,
    request: Request,
    readable: bool,
    authenticated_now: bool,
    out: &mut Vec<u8>,
  ) -> Result<Option<Box<Exchange>>, Refused> {
    let context = Context {
      session: &established.session,
      session_path: &established.base,
      youngest: index.youngest(),
      readable,
      authenticated_now,
      room: self.limits.report_room,
      item_bytes: self.limits.item_bytes,
      most_open: self.limits.most_open,
    };
    Ok(
      match Exchange::begin(&context, request, out).map_err(Refused::Report)? {
        Opened::Reporting(exchange) => Some(exchange),
        Opened::Failed(_) => None,
      },
    )
  }

  /// The command an authentication for it interrupted, run again with the principal the client
  /// proved and no trivial auth request before its answer.
  fn resume<'a, R>(
    &mut self,
    mut established: Box<Established>,
    interrupted: Interrupted,
    sources: &Sources<'_, R>,
    out: &mut Vec<u8>,
  ) -> Result<Turn<'a>, Refused>
  where
    R: reads::Repository + ?Sized,
  {
    match interrupted {
      Interrupted::Read { command } => self.read_again(established, &command, sources, out)?,
      Interrupted::Lock { command } => {
        self.phase = Phase::LockAsked(established, command, true);
        return Ok(Turn::LockAsked { consumed: 0 });
      }
      Interrupted::Commit { command, start } => {
        self.begin_commit(established, command, start, true, &sources.policy, out)?;
      }
      Interrupted::Update { command } => {
        let request = {
          let value = self.whole_item(&command)?;
          let (_, params) = command_of(&value).ok_or(Refused::Malformed)?;
          Request::read(RequestKind::Update, params).map_err(Refused::Report)?
        };
        let target = join_fspath(&established.base, &request.target);
        let readable = authz::may_read(established.compiled_read_rules(&sources.policy), &target);
        let begun = self.begin_report(&established, sources.index, request, readable, true, out)?;
        self.phase = match begun {
          Some(exchange) => Phase::Reporting(established, exchange),
          None => Phase::Commands(established),
        };
      }
    }
    Ok(Turn::Wrote)
  }

  /// A commit begun through A-655's entry point, `authenticated_now` once the client has just
  /// authenticated for it.
  fn begin_commit(
    &mut self,
    established: Box<Established>,
    command: Vec<u8>,
    start: CommitStart,
    authenticated_now: bool,
    policy: &Policy<'_>,
    out: &mut Vec<u8>,
  ) -> Result<(), Refused> {
    let begun = self.call_commit(
      &established,
      &command,
      &start,
      authenticated_now,
      policy,
      out,
    )?;
    match begun {
      Begun::Driving(drive) => self.phase = Phase::Committing(established, drive),
      Begun::Answered => self.phase = Phase::Commands(established),
      Begun::Close => return Err(Refused::Malformed),
      Begun::Authenticate => {
        let asked = if authenticated_now {
          None
        } else {
          self.ask(&established, policy, authz::Access::Write, out)?
        };
        match asked {
          Some(server) => {
            let interrupted = Box::new(Interrupted::Commit { command, start });
            self.phase = Phase::Authenticating(established, server, interrupted);
          }
          None => {
            ReportFailure::NotAuthorized
              .write(out)
              .map_err(Refused::Report)?;
            self.phase = Phase::Commands(established);
          }
        }
      }
    }
    Ok(())
  }

  /// A-655's entry point called with the kept `commit` item and the session as it stands.
  fn call_commit(
    &self,
    established: &Established,
    command: &[u8],
    start: &CommitStart,
    authenticated_now: bool,
    policy: &Policy<'_>,
    out: &mut Vec<u8>,
  ) -> Result<Begun, Refused> {
    let Ok(user) = principal_name(&established.session.principal) else {
      ReportFailure::NotAuthorized
        .write(out)
        .map_err(Refused::Report)?;
      return Ok(Begun::Answered);
    };
    let value = self.whole_item(command)?;
    let (_, params) = command_of(&value).ok_or(Refused::Malformed)?;
    let kept = &established.mechanisms;
    let session = commit_drive::Session {
      base_path: &established.base,
      repository_url: &established.repository_url,
      filesystem: policy.filesystem,
      user,
      anonymous: policy.anonymous,
      authenticated: policy.authenticated,
      can_authenticate: kept.tunnel_user.is_some() || kept.cram_md5,
      authz: policy.authz,
    };
    let start = Start {
      transaction: start.transaction.clone(),
      now: start.now,
      room: start.room,
      authenticated_now,
    };
    commit_drive::commit(params, &session, start, out).map_err(Refused::Item)
  }

  /// A commit's drive takes the client's next editor command.
  fn editing<'a, R>(
    &mut self,
    established: Box<Established>,
    mut drive: Box<Drive>,
    sources: &Sources<'_, R>,
    input: &'a [u8],
    out: &mut Vec<u8>,
  ) -> Result<Turn<'a>, Refused>
  where
    R: commit_drive::Green + ?Sized,
  {
    let (value, consumed) = match self.parse(input)? {
      Parsed::NeedMore { needed } => {
        self.phase = Phase::Committing(established, drive);
        return Ok(Turn::NeedMore { needed });
      }
      Parsed::Item { value, consumed } => (value, consumed),
    };
    let step = drive
      .command(
        sources.index,
        sources.repository,
        sources.locks,
        &value,
        out,
      )
      .map_err(Refused::Item)?;
    self.stepped(established, drive, step, consumed)
  }

  /// Where a drive's step leaves the connection, and its turn, `consumed` the bytes the step took.
  fn stepped<'a>(
    &mut self,
    established: Box<Established>,
    drive: Box<Drive>,
    step: Step,
    consumed: usize,
  ) -> Result<Turn<'a>, Refused> {
    Ok(match step {
      Step::Taken | Step::Refused | Step::Dropped => {
        self.phase = Phase::Committing(established, drive);
        Turn::Took { consumed }
      }
      Step::Ended => {
        self.phase = Phase::Commands(established);
        Turn::Took { consumed }
      }
      Step::Hook(due) => {
        self.phase = Phase::Hooked(established, drive);
        Turn::Hook { due, consumed }
      }
      Step::Accepted(work) => {
        self.phase = Phase::Accepted(established);
        Turn::Accepted { work, consumed }
      }
      Step::Close => return Err(Refused::Malformed),
    })
  }

  /// One of the client's commands after its commit was refused at `close-edit`: `abort-edit` or
  /// `success` ends the edit, anything else is dropped unread (`editorp.c` 1193-1208).
  fn dropping<'a>(
    &mut self,
    established: Box<Established>,
    input: &'a [u8],
  ) -> Result<Turn<'a>, Refused> {
    let (value, consumed) = match self.parse(input)? {
      Parsed::NeedMore { needed } => {
        self.phase = Phase::Dropping(established);
        return Ok(Turn::NeedMore { needed });
      }
      Parsed::Item { value, consumed } => (value, consumed),
    };
    let ends = match &value {
      Value::List(parts) => {
        matches!(parts.first(), Some(Value::Word(word)) if *word == ABORT_EDIT || *word == SUCCESS)
      }
      _ => false,
    };
    self.phase = if ends {
      Phase::Commands(established)
    } else {
      Phase::Dropping(established)
    };
    Ok(Turn::Took { consumed })
  }

  /// A read's answer resumed within the room.
  fn stream<'a, R>(
    &mut self,
    mut established: Box<Established>,
    mut stream: Box<Stream>,
    sources: &Sources<'_, R>,
    out: &mut Vec<u8>,
    budget: Budget,
  ) -> Result<Turn<'a>, Refused>
  where
    R: reads::Repository + ?Sized,
  {
    let authentication = established.reads_authentication(&sources.policy);
    let session = established.reads_session(&sources.policy, authentication);
    let progress = stream
      .resume(
        sources.index,
        sources.repository,
        &session,
        out,
        budget.room,
      )
      .map_err(Refused::Item)?;
    self.phase = match progress {
      reads::Progress::More => Phase::Streaming(established, stream),
      reads::Progress::Done => Phase::Commands(established),
      reads::Progress::Close => return Err(Refused::Aborted),
    };
    Ok(Turn::Wrote)
  }

  /// A report command's exchange, on.
  fn exchange<'a, R>(
    &mut self,
    mut established: Box<Established>,
    mut exchange: Box<Exchange>,
    sources: &Sources<'_, R>,
    input: &'a [u8],
    out: &mut Vec<u8>,
    budget: Budget,
  ) -> Result<Turn<'a>, Refused>
  where
    R: reads::Repository + ?Sized,
  {
    let drive_sources = report::drive::Sources {
      index: sources.index,
      repository: sources.repository,
      locks: sources.locks,
      now: sources.now,
      rules: established.compiled_read_rules(&sources.policy),
    };
    let turn = exchange
      .receive(&drive_sources, input, out, budget)
      .map_err(Refused::Report)?;
    Ok(match turn {
      ExchangeTurn::NeedMore { needed } => {
        self.phase = Phase::Reporting(established, exchange);
        Turn::NeedMore { needed }
      }
      ExchangeTurn::Took { consumed } => {
        self.phase = Phase::Reporting(established, exchange);
        Turn::Took { consumed }
      }
      ExchangeTurn::Wrote => {
        self.phase = Phase::Reporting(established, exchange);
        Turn::Wrote
      }
      ExchangeTurn::Answered { consumed } => {
        self.phase = Phase::Commands(established);
        Turn::Took { consumed }
      }
    })
  }

  /// The client's next item within the connection's bounds.
  fn parse<'a>(&self, input: &'a [u8]) -> Result<Parsed<'a>, Refused> {
    let bounds = Bounds {
      item_bytes: self.limits.item_bytes,
      word_bytes: LONGEST_WORD,
      depth: COMMAND_DEPTH,
      nodes: self
        .limits
        .item_bytes
        .checked_div(PARSED_ITEM_BYTES)
        .unwrap_or(0),
    };
    Parser::new(bounds).parse(input).map_err(Refused::Item)
  }

  /// A command's item kept whole, read again within the connection's bounds.
  fn whole_item<'b>(&self, item: &'b [u8]) -> Result<Value<'b>, Refused> {
    match self.parse(item)? {
      Parsed::Item { value, .. } => Ok(value),
      Parsed::NeedMore { .. } => Err(Refused::Malformed),
    }
  }
}

/// How a command the read commands handed over was answered.
enum Answered {
  /// Here: `reparent`, or a report command refused at once.
  Here,
  /// A report command begun: its exchange runs.
  Reporting(Box<Exchange>),
  /// An authentication for the command asked: it runs, then the command again.
  Authenticating(Box<handshake::Server>, Box<Interrupted>),
  /// `commit`: the caller's start is awaited.
  CommitAsked,
  /// A lock verb: the caller's judging is awaited.
  LockAsked,
  /// As a command not served yet.
  Unserved,
}

/// `reparent`: a trivial auth request, then the session moved to the URL's path, or refused for a
/// URL outside the repository (`serve.c` `reparent`). A URL that is no string, holds a NUL or is no
/// UTF-8 ends the connection, as the report's do.
fn reparent(
  established: &mut Established,
  params: &[Value<'_>],
  out: &mut Vec<u8>,
) -> Result<(), Refused> {
  let Some(Value::String(url)) = params.first() else {
    return Err(Refused::Malformed);
  };
  if url.contains(&0) {
    return Err(Refused::Malformed);
  }
  let url = std::str::from_utf8(url).map_err(|_| Refused::Malformed)?;
  write_trivial_auth(out).map_err(Refused::Report)?;
  match repository_path(&established.session.root_url, url) {
    Ok(base) => {
      established.base = base;
      write_empty_success(out).map_err(Refused::Report)
    }
    Err(failure) => failure.write(out).map_err(Refused::Report),
  }
}

/// A read command's answer: `get-lock` and `get-locks` from the lock table at the call's instant (A-693 part 1),
/// every other one by [`reads::answer`].
fn read_answer<R>(
  sources: &Sources<'_, R>,
  session: &reads::Session<'_>,
  value: &Value<'_>,
  out: &mut Vec<u8>,
) -> Result<Outcome, ItemError>
where
  R: reads::Repository + ?Sized,
{
  let locks = (sources.locks, sources.now);
  reads::answer_lock_read(
    sources.index,
    sources.repository,
    locks,
    session,
    value,
    out,
  )
  .unwrap_or_else(|| reads::answer(sources.index, sources.repository, session, value, out))
}

/// A command item's name, lent from the client's bytes, and its parameters.
fn command_of<'v, 'a>(value: &'v Value<'a>) -> Option<(&'a str, &'v [Value<'a>])> {
  let Value::List(parts) = value else {
    return None;
  };
  match parts.as_slice() {
    [Value::Word(name), Value::List(params), ..] => Some((*name, params)),
    _ => None,
  }
}

/// A repository's root URL as A-655 compares a copy source's with it, decoded as
/// `svn_path_uri_decode` decodes it (`serve.c` 1551); `None` for one that names no UTF-8 path.
fn decoded_url(root_url: &[u8]) -> Option<String> {
  let url = std::str::from_utf8(root_url).ok()?;
  String::from_utf8(uri_decode(url)).ok()
}

/// The principal's name as a commit records it in `svn:author` (`serve.c` 1536-1541): none for no
/// one; a name that is no UTF-8, which no `svn:` property may hold, is an error, and its commit is
/// refused "Authorization failed".
fn principal_name(principal: &Principal) -> Result<Option<&str>, std::str::Utf8Error> {
  match principal {
    Principal::Anonymous => Ok(None),
    Principal::User(name) => std::str::from_utf8(name).map(Some),
  }
}

/// Appends a command failure of `errors`, outermost first, each with no source file and line, as a
/// release build of `svnserve` writes them (`marshal.c` `svn_ra_svn__write_cmd_failure`).
fn write_failure(errors: &[(u32, String)], out: &mut Vec<u8>) -> Result<(), Refused> {
  let errors = errors
    .iter()
    .map(|(code, message)| {
      Value::List(vec![
        Value::Number(u64::from(*code)),
        Value::String(message.as_bytes()),
        Value::String(b""),
        Value::Number(0),
      ])
    })
    .collect();
  ra_svn::write(
    &Value::List(vec![Value::Word(FAILURE), Value::List(errors)]),
    out,
  )
  .map_err(Refused::Item)
}
