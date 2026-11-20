//! The commit's editor drive: each command checked as it arrives, then declared on the transaction, built a command at
//! a time, and `close-edit`'s verdict (§4.17.4 items 3.2 to 3.4 and "5. The wire", piece 7's commit; A-617).
//!
//! `svnserve` answers `commit` by driving Subversion's commit editor (`libsvn_repos/commit.c`) with the client's
//! editor commands (`libsvn_ra_svn/editorp.c`), over an FSFS transaction made with `SVN_FS_TXN_CHECK_LOCKS`, so that
//! FSFS checks the locks at each call that changes a path (`libsvn_fs_fs/tree.c`). This module is that drive with no
//! I/O, every check in `svnserve`'s order and every refusal in its words and numbers (Subversion's `1.14.x` branch at
//! `d434dc14`; the research notes' §14):
//! - **The tokens** (`editorp.c` 477-516): `open-root`, `add-dir` and `open-dir` name directory tokens, `add-file` and
//!   `open-file` file tokens; a token unknown or of the other kind is "Invalid file or dir token during edit", and a
//!   second `apply-textdelta` on one file, or a chunk or an end with none, is malformed data: each ends the connection
//!   ([`Step::Close`]). A file's token outlives its directory's `close-dir`, as a stock client's drive needs (it sends
//!   every text after closing the tree).
//! - **The checks** (`commit.c` 413-732, in its order): `open-root` past the youngest is "No such revision %ld (HEAD is
//!   %ld)", and the transaction's base is the youngest then. `delete-entry` of a path the transaction lacks is out of
//!   date; `delete-entry`, `open-file` and a directory's first property change carry the out-of-date test
//!   ([`check_out_of_date`]) when they name a revision. An add without a copy of a path the transaction holds is "File
//!   already exists"; a copy onto one is out of date unless its directory was itself copied in this drive, and then
//!   replaces it; a copy's source must lie in this repository and exist at its revision. `open-dir` of an absent path
//!   is "Path '%s' not present", `open-file` "File not found".
//! - **Locks at each call** (`tree.c`, `svn_fs_fs__allow_locked_operation`): a deletion, a directory's addition, a copy
//!   and a directory's property change meet every lock at or beneath the path; a file's addition, text and property
//!   change its own lock. Each is A-602's check ([`LockTable::verdict`]) over that one path, so a commit against
//!   another's lock is refused at the first call that meets it, before `close-edit`.
//! - **Texts** (`apply-textdelta`, its chunks, `textdelta-end`, `close-file`; `tree.c` `fs_apply_textdelta`): the
//!   base checksum, when sent, must equal the MD5 of the file's text in the transaction; the chunks are one svndiff
//!   stream, decoded and applied a window at a time as they arrive against that text (the svndiff codec, A-500), the
//!   new text's MD5 and SHA-1 taken as it is written; `close-file`'s checksum, when sent, must equal the MD5 of the
//!   file's text then (`E200014` otherwise, as `svn_checksum_mismatch_err` writes it).
//! - **Properties**: a change sets or deletes one property of the node's list; the new list is named by the green
//!   ([`Green::name_properties`]) and declared as one replacement of the node's list. First `svn_repos_fs_change_node_prop`'s
//!   checks (`fs-wrap.c` 297-338; A-655): `svn:mergeinfo` must hold no NUL and parse (A-605's parser; the parser's own
//!   words beneath `verify_mergeinfo`'s are owed with A-605's messages), a property `svn:wc:` or `svn:entry:` is never
//!   stored, and an `svn:` value must be UTF-8 without a carriage return, `svn:date` a date as `svn_time_from_cstring`
//!   reads one; a deletion is never refused.
//! - **Who may** (`commit.c` `check_authz`; A-655): each call asks A-606's rules, compiled for the principal
//!   ([`Setup::rules`]), what `svnserve`'s commit editor asks: a deletion a write at and beneath a directory and a write
//!   on its parent, an addition a write on the path and its parent and, for a copy, a read of the source, beneath it for
//!   a directory; `open-file` a read; a file's first change and a directory's a write. An added path holding a control
//!   character is refused first (`svn_repos__validate_new_path`).
//! - **The transaction**: each change is declared as it is checked, on the revision's changed-path rows, one per path,
//!   folded as FSFS folds them (a deletion then an addition is a replacement; a deletion drops the rows beneath it; an
//!   addition then a deletion leaves nothing). Every check reads the transaction as those rows leave it over the index
//!   at the base: a path's own row, else the nearest directory above it whose shape the drive changed (a deletion or a
//!   fresh directory hides it, a copy shows the source's), else the base's node; a node the drive changed, or changed
//!   beneath, has no created revision, as FSFS leaves a mutable node's unset. The drive borrows nothing between
//!   commands: the owner appends other commits to the index while drives run (the conflict case), so each command is
//!   handed the index, and reads at the base do not move. A-592's transaction ([`crate::index::Trial`]), every identity
//!   FSFS gives, is built from the rows once, by the verdict.
//! - **Hooks** (A-661): `open-root` checks its revision, then every revision property the transaction is begun with
//!   (`svn_repos_fs_change_txn_props`, `fs-wrap.c` 344-357, in name order where `svnserve` takes APR's hash order),
//!   then waits for start-commit ([`Step::Hook`], [`Due::StartCommit`]) before its root opens; `close-edit` waits for
//!   pre-commit ([`Due::PreCommit`]) before the verdict, as `svn_repos_fs_commit_txn` runs it before FSFS's merge. The
//!   caller runs the hook, which reads the transaction through [`Drive::pending`], and hands its answer to
//!   [`Drive::resume`]: passed, the command goes on; refused, the hook's errors are the command's failure, under the
//!   early-error rule.
//! - **`close-edit`** runs the verdict once pre-commit passed (A-592's [`verdict`]: FSFS's merge at commit against the
//!   head as it stands then, then the lock table's check with the commit's tokens, its releases decided). An accepted
//!   work, which names the head its verdict judged against and keeps no ephemeral `svn:txn-` property (`fs-wrap.c`
//!   72-89), is handed to the caller ([`Step::Accepted`]), who commits it ([`super::revision`]) and then writes
//!   `svnserve`'s answer ([`write_commit_info`]); a refusal is written.
//! - **The early-error rule** (`editorp.c` 1149-1208): a refused command's failure is written at once, the edit
//!   aborted, and every command after it read and dropped, malformed or not, until `abort-edit` or `success`, which end
//!   the drive without an answer. `abort-edit` while driving is answered `( success ( ) )`. A client's `failure` and
//!   `finish-replay` end the connection.
//!
//! Where this departs from `svnserve`, each for an input no stock client sends: a path that is not UTF-8 is refused
//! ([`DriveRefusal::PathSyntax`]), where FSFS stores any bytes; a copy source whose URL continues the repository's
//! root without a `/` is from a different repository, where `svnserve`'s prefix test reads the rest as a path; FSFS's
//! "already exists" message names its filesystem's directory on disk, and slates names the green
//! ([`Setup::filesystem`]); a file closed with its text delta still open keeps the text it had. The room the work's new
//! texts and property lists may hold is the caller's, derived from its admission, and refused past it.
//!
//! No I/O, no clock (the caller's instant), no randomness, no panic.

use std::collections::BTreeMap;

use aws_lc_rs::digest::{Context, MD5_FOR_LEGACY_USE_ONLY, SHA1_FOR_LEGACY_USE_ONLY};

use super::edit::{CommitRequest, CopiedFrom, EditCommand, EditRefusal, LockTokensRefusal};
use super::{CommitRefusal, check_out_of_date, verdict};
use crate::authz::{Access, Authz, Principal, PrincipalRules, RepositoryPath};
use crate::index::{
  Action, ChangedPath, ContentIdentity, CopySource, Index, IndexRefusal, NodeKind, PropertyList,
  Revision, RevisionRecord, Text,
};
use crate::lock::{AprTime, CommitLocks, LockEntry, LockRefusal, LockTable};
use crate::mergeinfo::{Mergeinfo, MergeinfoRefusal};
use crate::ra_svn::{ItemError, Value, write};
use crate::svndiff::{self, Decoded, Decoder, InstructionFault, SvndiffError};

/// The word of a command's success.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_response`.
const SUCCESS: &str = "success";

/// The word of a command's failure.
/// Format: `marshal.c` `svn_ra_svn__write_cmd_failure`.
const FAILURE: &str = "failure";

/// The command that ends a drive after a refusal, and the one a drive's abort is.
/// Format: `editorp.c` 1205-1206.
const ABORT_EDIT: &str = "abort-edit";

/// The root's path.
/// Format: Subversion's fspath of the root.
const ROOT: &str = "/";

/// A path's separator.
/// Format: Subversion's relpaths and fspaths.
const SEPARATOR: char = '/';

/// The segment that names its own directory, which `svn_relpath_canonicalize` drops.
/// Format: `dirent_uri.c` canonicalization.
const CURRENT: &str = ".";

/// The escape of a URL's percent-encoded byte.
/// Format: RFC 3986 §2.1, which `svn_path_uri_decode` reads.
const PERCENT: u8 = b'%';

/// The base of a percent escape's two digits, and of a checksum's.
/// Format: RFC 3986 §2.1; `svn_checksum_parse_hex`.
const HEX: u8 = 16;

/// A percent escape's bytes: `%` and two hexadecimal digits.
/// Format: RFC 3986 §2.1.
const ESCAPE_BYTES: usize = 3;

/// An MD5 digest's bytes, which a checksum's first 32 hexadecimal digits name.
/// Format: RFC 1321 §3.5; `svn_checksum_parse_hex` reads `DIGESTSIZE(svn_checksum_md5)` bytes and ignores the rest.
const MD5_BYTES: usize = 16;

/// The property whose changes FSFS flags as `mergeinfo-mod`.
/// Format: `svn_props.h` `SVN_PROP_MERGEINFO`.
const MERGEINFO: &[u8] = b"svn:mergeinfo";

/// The revision property a commit's log message is.
/// Format: `svn_props.h` `SVN_PROP_REVISION_LOG`; `serve.c` 1526-1534.
const REVISION_LOG: &[u8] = b"svn:log";

/// The revision property the commit's principal is.
/// Format: `svn_props.h` `SVN_PROP_REVISION_AUTHOR`; `serve.c` 1536-1541.
const REVISION_AUTHOR: &[u8] = b"svn:author";

/// `SVN_ERR_FS_GENERAL`: a copy from another repository.
/// Format: the filesystem category's start, 120,000 + 8 × 5,000 = 160,000 (`svn_error_codes.h` 581-582).
const SVN_ERR_FS_GENERAL: u32 = 160_000;

/// `SVN_ERR_FS_PATH_SYNTAX`: a path slates cannot hold.
/// Format: the filesystem category's start plus 5 (`svn_error_codes.h`).
const SVN_ERR_FS_PATH_SYNTAX: u32 = 160_005;

/// `SVN_ERR_FS_NO_SUCH_REVISION`: `open-root` or a copy naming a revision past the youngest.
/// Format: the filesystem category's start plus 6.
const SVN_ERR_FS_NO_SUCH_REVISION: u32 = 160_006;

/// `SVN_ERR_FS_NOT_FOUND`: a path the transaction, or a copy's revision, does not hold.
/// Format: the filesystem category's start plus 13 (`svn_error_codes.h` 633-634).
const SVN_ERR_FS_NOT_FOUND: u32 = 160_013;

/// `SVN_ERR_FS_NOT_DIRECTORY`: `open-dir` of an absent path.
/// Format: the filesystem category's start plus 16 (`svn_error_codes.h` 645-646).
const SVN_ERR_FS_NOT_DIRECTORY: u32 = 160_016;

/// `SVN_ERR_FS_ALREADY_EXISTS`: an add without a copy of a path the transaction holds.
/// Format: the filesystem category's start plus 20 (`svn_error_codes.h` 661-662).
const SVN_ERR_FS_ALREADY_EXISTS: u32 = 160_020;

/// `SVN_ERR_REPOS_BAD_ARGS`: `close-edit` before `open-root`.
/// Format: the repository category's start, 120,000 + 9 × 5,000 = 165,000, plus 2 (`svn_error_codes.h` 916-917).
const SVN_ERR_REPOS_BAD_ARGS: u32 = 165_002;

/// `SVN_ERR_BAD_CHECKSUM_PARSE`: a checksum that is not 32 hexadecimal digits.
/// Format: the bad-argument category's start, 120,000 + 5,000 = 125,000, plus 12 (`svn_error_codes.h` 219-220).
const SVN_ERR_BAD_CHECKSUM_PARSE: u32 = 125_012;

/// `SVN_ERR_CHECKSUM_MISMATCH`: a base or a resulting text whose MD5 is not the client's.
/// Format: the miscellaneous category's start, 120,000 + 16 × 5,000 = 200,000, plus 14 (`svn_error_codes.h` 1337).
const SVN_ERR_CHECKSUM_MISMATCH: u32 = 200_014;

/// `SVN_ERR_BASE`: the digest library failing, which no input causes.
/// Format: the miscellaneous category's start.
const SVN_ERR_BASE: u32 = 200_000;

/// `SVN_ERR_RA_SVN_UNKNOWN_CMD`: a command no handler has, answered as a command failure.
/// Format: the ra_svn category's start, 120,000 + 18 × 5,000 = 210,000, plus 1.
const SVN_ERR_RA_SVN_UNKNOWN_CMD: u32 = 210_001;

/// `SVN_ERR_RA_NOT_AUTHORIZED`: a commit the principal may not make.
/// Format: the ra category's start, 120,000 + 10 × 5,000 = 170,000, plus 1 (`svn_error_codes.h`).
const SVN_ERR_RA_NOT_AUTHORIZED: u32 = 170_001;

/// `SVN_ERR_RA_NOT_AUTHORIZED`'s own words, which `svnserve` writes for the error it makes without a message
/// (`error_create_and_log(SVN_ERR_RA_NOT_AUTHORIZED, NULL, NULL, b)`, `svn_err_best_message`).
const NOT_AUTHORIZED: &str = "Authorization failed";

/// `SVN_ERR_RA_SVN_REQUEST_SIZE`: a work past its room.
/// Format: the ra_svn category's start plus 9 (`svn_error_codes.h` 1601-1602).
const SVN_ERR_RA_SVN_REQUEST_SIZE: u32 = 210_009;

/// `SVN_ERR_BAD_DATE`: an `svn:date` that is no date.
/// Format: the bad-argument category's start plus 3 (`svn_error_codes.h`).
const SVN_ERR_BAD_DATE: u32 = 125_003;

/// `SVN_ERR_BAD_PROPERTY_VALUE`: an `svn:` property's value refused.
/// Format: the bad-argument category's start plus 5 (`svn_error_codes.h`).
const SVN_ERR_BAD_PROPERTY_VALUE: u32 = 125_005;

/// `SVN_ERR_BAD_PROPERTY_VALUE_EOL`: an `svn:` property's value holding a carriage return.
/// Format: the bad-argument category's start plus 17 (`svn_error_codes.h`).
const SVN_ERR_BAD_PROPERTY_VALUE_EOL: u32 = 125_017;

/// `SVN_ERR_MERGEINFO_PARSE_ERROR`: `svn:mergeinfo` refused at commit.
/// Format: the miscellaneous category's start plus 20 (`svn_error_codes.h`).
const SVN_ERR_MERGEINFO_PARSE_ERROR: u32 = 200_020;

/// `SVN_ERR_AUTHZ_UNREADABLE`: a read the rules deny.
/// Format: the authorization category's start, 120,000 + 20 × 5,000 = 220,000, plus 1 (`svn_error_codes.h`).
const SVN_ERR_AUTHZ_UNREADABLE: u32 = 220_001;

/// `SVN_ERR_AUTHZ_UNWRITABLE`: a write the rules deny.
/// Format: the authorization category's start plus 4 (`svn_error_codes.h`).
const SVN_ERR_AUTHZ_UNWRITABLE: u32 = 220_004;

/// The prefix of Subversion's own properties.
/// Format: `svn_props.h` `SVN_PROP_PREFIX`.
const SVN_PREFIX: &[u8] = b"svn:";

/// The prefix of the working copy's properties, which the repository never stores.
/// Format: `svn_props.h` `SVN_PROP_WC_PREFIX`; `properties.c` `svn_property_kind2`.
const WC_PREFIX: &[u8] = b"svn:wc:";

/// The prefix of the entry properties, which the repository makes and never stores as sent.
/// Format: `svn_props.h` `SVN_PROP_ENTRY_PREFIX`; `properties.c` `svn_property_kind2`.
const ENTRY_PREFIX: &[u8] = b"svn:entry:";

/// The property whose value must be a date.
/// Format: `svn_props.h` `SVN_PROP_REVISION_DATE`; `fs-wrap.c` 250.
const REVISION_DATE: &[u8] = b"svn:date";

/// What begins the name of an ephemeral transaction property, which the client sends with its revision properties
/// and no revision keeps (`svn:txn-user-agent`, `svn:txn-client-compat-version`).
/// Format: `SVN_PROP_TXN_PREFIX`, `SVN_PROP_PREFIX "txn-"` (`svn_props.h` 706).
const TXN_PREFIX: &[u8] = b"svn:txn-";

/// The carriage return an `svn:` property may not hold.
/// Format: `fs-wrap.c` 237, `strchr(value->data, '\r')`.
const CARRIAGE_RETURN: u8 = b'\r';

/// The byte that ends a C string.
/// Format: C's terminator, where `strchr`, `strlen` and `svn_time_from_cstring` stop.
const NUL: u8 = 0;

/// The separator after each number of a date, in order.
/// Format: `time.c` `TIMESTAMP_FORMAT`, `"%04d-%02d-%02dT%02d:%02d:%02d.%06dZ"`, as `svn_time_from_cstring` reads it.
const DATE_SEPARATORS: [u8; 7] = *b"--T::.Z";

/// How many numbers a date has: one before each separator.
const DATE_FIELDS: usize = DATE_SEPARATORS.len();

/// The base `svn__strtoul` reads in.
/// Format: decimal digits (`string.c` `svn__strtoul`).
const DECIMAL: u64 = 10;

/// The year `struct tm`'s years count from.
/// Format: `time.c`, `tm_year -= 1900`.
const TM_YEAR_BASE: i64 = 1900;

/// The days from the 1st of March to the 1st of each month, January first, as APR counts a year from March.
/// Format: APR 1.7.5 `time/unix/time.c` `apr_time_exp_get`'s `dayoffset`.
const DAY_OFFSET: [i64; 12] = [306, 337, 0, 31, 61, 92, 122, 153, 184, 214, 245, 275];

/// The days of a common year.
/// Format: the Gregorian calendar, as `apr_time_exp_get` counts.
const DAYS_PER_YEAR: i64 = 365;

/// The years between leap years.
/// Format: the Gregorian calendar, `year / 4` in `apr_time_exp_get`.
const LEAP_EVERY: i64 = 4;

/// The years of a century, whose first is no leap year unless the fourth.
/// Format: the Gregorian calendar, `year / 100` in `apr_time_exp_get`.
const CENTURY: i64 = 100;

/// The offset that counts 2000's leap day from 1900 in `(year / 100 + 3) / 4`.
/// Format: `apr_time_exp_get`.
const CENTURY_LEAP_OFFSET: i64 = 3;

/// The days from the 1st of March 1900 to the 1st of January 1970.
/// Format: `apr_time_exp_get`, "1 jan 1970 is 25508 days since 1 mar 1900".
const EPOCH_DAYS: i64 = 25_508;

/// The hours of a day.
/// Format: `apr_time_exp_get`.
const HOURS_PER_DAY: i64 = 24;

/// The minutes of an hour and the seconds of a minute.
/// Format: `apr_time_exp_get`.
const SIXTY: i64 = 60;

/// `SVN_ERR_FS_CONFLICT`'s message, as FSFS's merge writes it.
/// Format: `tree.c` `conflict_err`.
const CONFLICT_AT: &str = "Conflict at";

/// What the drive reads from the green beyond the index, and how the green names what the work makes (§4.17.4 item 1:
/// texts and property lists are content, named by their identity).
pub trait Green {
  /// The text held as `identity`; `None` when the green does not hold it.
  fn text(&self, identity: &ContentIdentity) -> Option<&[u8]>;

  /// The property list held as `identity`, every property, in any order; `None` when the green does not hold it.
  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>>;

  /// The identity the green names `text` by.
  fn name_text(&self, text: &[u8]) -> ContentIdentity;

  /// The identity the green names a property list by, given sorted by name, each name once.
  fn name_properties(&self, properties: &[(Vec<u8>, Vec<u8>)]) -> ContentIdentity;
}

/// What a drive runs with: what `commit` brought and the session it came on (`serve.c` `commit` 1470-1556).
#[derive(Clone, Debug)]
pub struct Setup {
  /// The session's path in the repository, canonical (`svnserve`'s `fs_path`): every editor path is joined to it.
  pub base_path: String,
  /// The repository's root URL, canonical and decoded: a copy's source must begin with it (`commit.c` 346-353).
  pub repository_url: String,
  /// The green's name, which FSFS's "File already exists" message names where FSFS names its directory on disk.
  pub filesystem: String,
  /// The transaction's name (§4.17.4 item 3.1), which FSFS's "File not found" message names.
  pub transaction: String,
  /// The principal, the lock tokens the commit presented with their full paths, and whether it keeps its locks.
  pub locks: CommitLocks,
  /// The owner's instant, at which locks are judged expired or not.
  pub now: AprTime,
  /// How many bytes the work's new texts and property lists may hold, with the deltas being read: the caller's room,
  /// derived from its admission.
  pub room: usize,
  /// The authorization rules the principal meets in this repository (A-606), compiled; `None` when the repository
  /// names no authz file, which `svnserve` takes as every path allowed (`serve.c` `authz_check_access`).
  pub rules: Option<PrincipalRules>,
  /// The revision properties the commit records but its date ([`revision_properties`]).
  pub revision_properties: Vec<(Vec<u8>, Vec<u8>)>,
}

/// The session a `commit` arrives on, as the svn:// connection task holds it (A-656): what `svnserve`'s
/// `must_have_access` and commit editor read from its server baton (`serve.c` 571-575, 760-850).
#[derive(Clone, Copy, Debug)]
pub struct Session<'s> {
  /// The session's path in the repository, canonical (`fs_path`).
  pub base_path: &'s str,
  /// The repository's root URL, canonical and decoded.
  pub repository_url: &'s str,
  /// The green's name ([`Setup::filesystem`]).
  pub filesystem: &'s str,
  /// The authenticated principal; `None` for an anonymous session.
  pub user: Option<&'s str>,
  /// What `svnserve.conf` gives an anonymous session (`anon-access`).
  pub anonymous: Access,
  /// What it gives an authenticated one (`auth-access`).
  pub authenticated: Access,
  /// Whether the client could authenticate: a tunnel's user, a password database or SASL (`serve.c` 834-835).
  pub can_authenticate: bool,
  /// The authorization rules and the name they know this repository by (`authz_repos_name`); `None` when the
  /// repository names no authz file.
  pub authz: Option<(&'s Authz, &'s [u8])>,
}

/// What the owner gives a commit as it begins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Start {
  /// The transaction's name (§4.17.4 item 3.1).
  pub transaction: String,
  /// The owner's instant.
  pub now: AprTime,
  /// The room the work's texts and property lists may hold ([`Setup::room`]).
  pub room: usize,
  /// Whether the client has just authenticated for this commit ([`Begun::Authenticate`]): then no trivial
  /// authentication request is written (`serve.c` 832-849).
  pub authenticated_now: bool,
}

/// How a `commit` began ([`commit`]).
pub enum Begun {
  /// Its answer is written: hand the drive every item that follows ([`Drive::command`]).
  Driving(Box<Drive>),
  /// The client must authenticate for write first (`serve.c` 832-836), nothing written: run the authentication exchange
  /// (A-599's mechanisms) and call [`commit`] again with the session it proved, [`Start::authenticated_now`] set.
  Authenticate,
  /// Refused and over: its failure written.
  Answered,
  /// Parameters `svnserve` cannot read: the connection ends, nothing written.
  Close,
}

impl std::fmt::Debug for Begun {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(match self {
      Self::Driving(_) => "Driving",
      Self::Authenticate => "Authenticate",
      Self::Answered => "Answered",
      Self::Close => "Close",
    })
  }
}

/// The one entry point for `commit` (A-655; the svn:// connection task, A-656, calls it with the command's
/// parameters, then hands the drive every item that follows): reads them as `serve.c` reads them (A-617 part 2a);
/// decides the principal's access as `must_have_access` does (`serve.c` 807-850): allowed, the trivial authentication
/// request; an anonymous session that could authenticate for write, [`Begun::Authenticate`]; else "Authorization
/// failed"; then the lock tokens (`add_lock_tokens`), then `( success ( ) )`, the drive set up with the principal's
/// compiled rules and the revision properties.
///
/// # Errors
/// [`ItemError`] when an answer cannot be written, which none here can give.
pub fn commit(
  params: &[Value<'_>],
  session: &Session<'_>,
  start: Start,
  out: &mut Vec<u8>,
) -> Result<Begun, ItemError> {
  let Ok(request) = CommitRequest::read(params) else {
    return Ok(Begun::Close);
  };
  let sends_tokens = request
    .rest
    .as_ref()
    .is_some_and(|rest| !rest.lock_tokens.is_empty());
  if may_write(session, None, sends_tokens) {
    if !start.authenticated_now {
      write(&trivial_auth(), out)?;
    }
  } else if session.user.is_none()
    && session.authenticated == Access::Write
    && session.can_authenticate
    && !start.authenticated_now
  {
    return Ok(Begun::Authenticate);
  } else {
    write_failure_words(SVN_ERR_RA_NOT_AUTHORIZED, NOT_AUTHORIZED, out)?;
    return Ok(Begun::Answered);
  }
  let Some(locks) = presented_locks(&request, session, out)? else {
    return Ok(Begun::Answered);
  };
  write(
    &Value::List(vec![Value::Word(SUCCESS), Value::List(Vec::new())]),
    out,
  )?;
  let principal = Principal::named(session.user.map(str::as_bytes));
  let setup = Setup {
    base_path: session.base_path.to_owned(),
    repository_url: session.repository_url.to_owned(),
    filesystem: session.filesystem.to_owned(),
    transaction: start.transaction,
    locks,
    now: start.now,
    room: start.room,
    rules: session
      .authz
      .map(|(authz, name)| authz.rules(name, principal)),
    revision_properties: revision_properties(&request, session.user),
  };
  Ok(Begun::Driving(Box::new(Drive::new(setup))))
}

/// Why a command was refused, each written as `svnserve` writes it ([`DriveRefusal::subversion_code`],
/// [`Drive::message`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriveRefusal {
  /// The commit rule's refusals: out of date, no such revision, a conflict at `close-edit`, a lock in the way, or the
  /// index refusing a change (A-592).
  Commit(CommitRefusal),
  /// `open-root` naming a revision past the youngest (`commit.c` 428-431).
  PastHead {
    /// The revision named.
    revision: Revision,
    /// The youngest.
    youngest: Revision,
  },
  /// `open-dir` of a path the transaction does not hold (`commit.c` 550-554), named as the client named it.
  NotPresent {
    /// The path, relative to the session.
    path: String,
  },
  /// A path the transaction does not hold (`SVN_FS__NOT_FOUND`, `svn_fs_util.h` 82-91).
  NotFound {
    /// The full path.
    path: String,
  },
  /// A copy's source absent at its revision (`SVN_FS__NOT_FOUND` on a revision root).
  SourceNotFound {
    /// The source's path.
    path: String,
    /// The source's revision.
    revision: Revision,
  },
  /// A copy's revision past the youngest (`svn_fs_revision_root`).
  SourceRevision {
    /// The revision named.
    revision: Revision,
  },
  /// An add without a copy of a path the transaction holds (`SVN_FS__ALREADY_EXISTS`, `svn_fs_util.h` 98-107).
  AlreadyExists {
    /// The full path.
    path: String,
  },
  /// A copy whose source URL is not in this repository (`commit.c` 346-353).
  ForeignSource {
    /// The URL, decoded.
    url: String,
  },
  /// A base checksum that is not the MD5 of the file's text in the transaction (`tree.c` `fs_apply_textdelta`).
  BaseChecksum {
    /// The full path.
    path: String,
    /// The client's.
    expected: [u8; 16],
    /// The text's.
    actual: [u8; 16],
  },
  /// A resulting text whose MD5 is not `close-file`'s (`commit.c` 679-694).
  ResultChecksum {
    /// The full path.
    path: String,
    /// The client's.
    expected: [u8; 16],
    /// The text's.
    actual: [u8; 16],
  },
  /// A checksum that is not 32 hexadecimal digits (`svn_checksum_parse_hex`, which gives no message).
  ChecksumSyntax,
  /// The text delta refused by the svndiff codec, in `svndiff.c`'s words.
  Delta(SvndiffError),
  /// `close-edit` with no transaction, `open-root` never sent (`commit.c` 811-813).
  NoTransaction,
  /// A command no handler has (`editorp.c` 1127-1132).
  UnknownCommand {
    /// Its name.
    name: String,
  },
  /// The commit's lock tokens are not a list of pairs of strings (`serve.c` `add_lock_tokens` 1360-1409).
  LockTokens(LockTokensRefusal),
  /// A path slates cannot hold: not UTF-8 (module documentation).
  PathSyntax {
    /// The path, lossily.
    path: String,
  },
  /// The work's new texts and property lists past [`Setup::room`].
  Room {
    /// The room.
    room: usize,
  },
  /// The digest library failed, which no input causes.
  DigestFailed,
  /// An added path holding a control character (`path.c` `svn_path_check_valid`).
  ControlCharacter {
    /// The first control character.
    byte: u8,
    /// The path as the client sent it.
    path: Vec<u8>,
  },
  /// A property Subversion keeps for itself, `svn:wc:` or `svn:entry:`, set through the repository (`fs-wrap.c` 213).
  NotRegular {
    /// Its name.
    name: String,
  },
  /// An `svn:` property's value that is not UTF-8 (`fs-wrap.c` 227).
  NotUtf8 {
    /// Its name.
    name: String,
  },
  /// An `svn:` property's value holding a carriage return (`fs-wrap.c` 237), two errors on the wire.
  NonLf {
    /// Its name.
    name: String,
  },
  /// An `svn:date` that is no date (`fs-wrap.c` 250-259), two errors on the wire.
  BadDate,
  /// `svn:mergeinfo` holding a NUL (`fs-wrap.c` `verify_mergeinfo` 311).
  MergeinfoTerminator {
    /// The node's path.
    path: String,
  },
  /// `svn:mergeinfo` that does not parse (`fs-wrap.c` `verify_mergeinfo` 317-322); the parser's own words, beneath
  /// these, are owed with A-605's messages.
  MergeinfoInvalid {
    /// The node's path.
    path: String,
    /// Why A-605's parser refused it.
    refusal: MergeinfoRefusal,
  },
  /// A write the authorization rules deny (`commit.c` `check_authz`).
  Unwritable,
  /// A read the authorization rules deny (`commit.c` `check_authz`).
  Unreadable,
}

impl DriveRefusal {
  /// The number of the Subversion error this refusal is answered with.
  pub fn subversion_code(&self) -> u32 {
    match self {
      Self::Commit(refusal) => refusal.subversion_code(),
      Self::PastHead { .. } | Self::SourceRevision { .. } => SVN_ERR_FS_NO_SUCH_REVISION,
      Self::NotPresent { .. } => SVN_ERR_FS_NOT_DIRECTORY,
      Self::NotFound { .. } | Self::SourceNotFound { .. } => SVN_ERR_FS_NOT_FOUND,
      Self::AlreadyExists { .. } => SVN_ERR_FS_ALREADY_EXISTS,
      Self::ForeignSource { .. } => SVN_ERR_FS_GENERAL,
      Self::BaseChecksum { .. } | Self::ResultChecksum { .. } => SVN_ERR_CHECKSUM_MISMATCH,
      Self::ChecksumSyntax => SVN_ERR_BAD_CHECKSUM_PARSE,
      Self::Delta(refusal) => refusal.subversion_code(),
      Self::NoTransaction => SVN_ERR_REPOS_BAD_ARGS,
      Self::UnknownCommand { .. } => SVN_ERR_RA_SVN_UNKNOWN_CMD,
      Self::LockTokens(refusal) => refusal.subversion_code(),
      Self::PathSyntax { .. } => SVN_ERR_FS_PATH_SYNTAX,
      Self::Room { .. } => SVN_ERR_RA_SVN_REQUEST_SIZE,
      Self::DigestFailed => SVN_ERR_BASE,
      Self::ControlCharacter { .. } => SVN_ERR_FS_PATH_SYNTAX,
      Self::NotRegular { .. } => SVN_ERR_REPOS_BAD_ARGS,
      Self::NotUtf8 { .. } | Self::NonLf { .. } | Self::BadDate => SVN_ERR_BAD_PROPERTY_VALUE,
      Self::MergeinfoTerminator { .. } | Self::MergeinfoInvalid { .. } => {
        SVN_ERR_MERGEINFO_PARSE_ERROR
      }
      Self::Unwritable => SVN_ERR_AUTHZ_UNWRITABLE,
      Self::Unreadable => SVN_ERR_AUTHZ_UNREADABLE,
    }
  }
}

impl From<CommitRefusal> for DriveRefusal {
  fn from(refusal: CommitRefusal) -> Self {
    Self::Commit(refusal)
  }
}

impl From<IndexRefusal> for DriveRefusal {
  fn from(refusal: IndexRefusal) -> Self {
    Self::Commit(CommitRefusal::Index(refusal))
  }
}

impl From<LockRefusal> for DriveRefusal {
  fn from(refusal: LockRefusal) -> Self {
    Self::Commit(CommitRefusal::Locked(refusal))
  }
}

impl From<SvndiffError> for DriveRefusal {
  fn from(refusal: SvndiffError) -> Self {
    Self::Delta(refusal)
  }
}

/// What a command's handling stopped at.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stop {
  /// A command failure, written, the edit aborted.
  Refused(DriveRefusal),
  /// An error outside a command error: the connection ends (`editorp.c` 1110, 1175).
  Close,
}

impl From<DriveRefusal> for Stop {
  fn from(refusal: DriveRefusal) -> Self {
    Self::Refused(refusal)
  }
}

impl From<CommitRefusal> for Stop {
  fn from(refusal: CommitRefusal) -> Self {
    Self::Refused(refusal.into())
  }
}

impl From<IndexRefusal> for Stop {
  fn from(refusal: IndexRefusal) -> Self {
    Self::Refused(refusal.into())
  }
}

impl From<LockRefusal> for Stop {
  fn from(refusal: LockRefusal) -> Self {
    Self::Refused(refusal.into())
  }
}

impl From<SvndiffError> for Stop {
  fn from(refusal: SvndiffError) -> Self {
    Self::Refused(refusal.into())
  }
}

/// What the drive made of one item.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
  /// The command was taken; nothing is written.
  Taken,
  /// The command was refused: its failure is written, and the drive reads and drops every item until `abort-edit`.
  Refused,
  /// An item read and dropped after a refusal.
  Dropped,
  /// The drive ended: at `abort-edit` while driving, its `( success ( ) )` written; or after a refusal, at `abort-edit`
  /// or `success`, nothing written.
  Ended,
  /// `close-edit` accepted the work. Nothing is written: the caller commits it, then writes the answer
  /// ([`write_commit_info`]).
  Accepted(Box<Work>),
  /// The command waits for a hook `svnserve` runs inside it (A-661): run it, then hand its answer to
  /// [`Drive::resume`] before any other item. Nothing is written.
  Hook(Due),
  /// An item `svnserve` refuses outside a command error: the connection ends, nothing written.
  Close,
}

/// A hook a command waits for, where `svnserve` runs it (A-661; the hook glue runs it and [`Drive::resume`] takes its
/// answer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Due {
  /// start-commit, which `open-root` runs once its revision is checked, as the transaction is made on the youngest
  /// revision (`commit.c` `open_root` 437-441, `fs-wrap.c` `svn_repos_fs_begin_txn_for_commit2` 151-178, the
  /// revision properties checked first).
  StartCommit {
    /// The transaction's name.
    transaction: String,
  },
  /// pre-commit, which `close-edit` runs before FSFS's merge and its lock check (`commit.c` `close_edit` 816,
  /// `fs-wrap.c` `svn_repos_fs_commit_txn` 69-71): the verdict judges the work against the head as it stands once the
  /// hook passed.
  PreCommit {
    /// The transaction's name.
    transaction: String,
  },
}

/// What a hook reads of the transaction its command waits on ([`Drive::pending`], A-661): what FSFS's read-only view of
/// a transaction (`svnlook -t`'s, owed) is built over.
#[derive(Clone, Copy, Debug)]
pub struct Pending<'drive> {
  /// The transaction's name.
  pub transaction: &'drive str,
  /// The revision it was begun on: the youngest at `open-root`.
  pub base: Revision,
  /// The revision properties it was begun with, the client's ephemeral `svn:txn-` ones among them, each name once in
  /// name order.
  pub revision_properties: &'drive [(Vec<u8>, Vec<u8>)],
  /// Its changed-path rows so far, by path.
  pub rows: &'drive BTreeMap<String, ChangedPath>,
  /// The texts it made, by identity.
  pub texts: &'drive BTreeMap<ContentIdentity, Vec<u8>>,
  /// The property lists it made, by identity, each sorted by name.
  pub property_lists: &'drive BTreeMap<ContentIdentity, Vec<(Vec<u8>, Vec<u8>)>>,
}

/// An accepted work: what the caller commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Work {
  /// The youngest revision its verdict judged it against: it commits as the revision after, and only while this is
  /// still the youngest (FSFS makes the head it merged the transaction's base, `tree.c` 2291, and its commit refuses
  /// a base no longer the youngest, `transaction.c` 3784-3786).
  pub head: Revision,
  /// The revision's record: the transaction's name and its changed-path rows.
  pub record: RevisionRecord,
  /// The lock entries the verdict decided, the presented locks released among them.
  pub lock_entries: Vec<LockEntry>,
  /// The revision's properties but its date: the client's, or `svn:log` alone, and `svn:author` the principal's
  /// (`serve.c` 1526-1541), each name once in name order; the ephemeral `svn:txn-` properties the client sent are
  /// dropped once pre-commit passed, as `svn_repos_fs_commit_txn` drops them before it commits (`fs-wrap.c` 72-89).
  pub revision_properties: Vec<(Vec<u8>, Vec<u8>)>,
  /// The texts the work made, by identity.
  pub texts: BTreeMap<ContentIdentity, Vec<u8>>,
  /// The property lists the work made, by identity, each sorted by name.
  pub property_lists: BTreeMap<ContentIdentity, Vec<(Vec<u8>, Vec<u8>)>>,
  /// The commit's locks: who commits, the tokens it presented and whether it keeps them, which the owner's step judges
  /// the work again with where its verdict's facts moved (A-655 part 2b; A-698).
  pub locks: CommitLocks,
}

/// Where the drive is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
  /// Taking commands.
  Driving,
  /// A command waiting for its hook's answer ([`Drive::resume`]).
  Waiting(Pause),
  /// After a refusal: reading and dropping until `abort-edit` or `success`.
  Dropping,
  /// Ended.
  Done,
}

/// The command a hook paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pause {
  /// `open-root`, for start-commit.
  OpenRoot,
  /// `close-edit`, for pre-commit.
  CloseEdit,
}

/// What `open-root` makes once start-commit passed: the root directory's token, and the revision the client holds it
/// at.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingRoot {
  /// The root's token.
  token: Vec<u8>,
  /// The revision the client holds the root at, when it said.
  revision: Option<Revision>,
}

/// A directory token's directory (`commit.c` `struct dir_baton`).
#[derive(Clone, Debug)]
struct Directory {
  /// Its full path.
  path: String,
  /// Whether this drive copied it, or a directory above it.
  copied: bool,
  /// The revision the client holds it at, when it said.
  base: Option<Revision>,
  /// Whether its first property change was checked (`checked_write`): from the start for one the drive added.
  checked: bool,
}

/// A text delta being read (`editorp.c`'s `dstream`).
struct Delta {
  /// The svndiff reader.
  decoder: Decoder,
  /// The bytes received and not yet read.
  pending: Vec<u8>,
  /// The text it applies to: its identity, or `None` for an empty one.
  source: Option<ContentIdentity>,
  /// The new text, as written so far.
  target: Vec<u8>,
  /// Its MD5 and SHA-1, taken as it is written.
  digests: Digests,
}

/// A file token's file (`commit.c` `struct file_baton`).
struct File {
  /// Its full path.
  path: String,
  /// Its text delta, while one is open.
  delta: Option<Delta>,
  /// Whether its write was checked (`checked_write`): from the start for one the drive added.
  checked: bool,
}

/// What a token names.
enum Baton {
  /// A directory.
  Directory(Directory),
  /// A file.
  File(File),
}

/// A running MD5 and SHA-1, through aws-lc-rs (the workspace's one cryptographic library).
struct Digests {
  md5: Context,
  sha1: Context,
}

impl Digests {
  /// Fresh digests.
  fn new() -> Result<Self, DriveRefusal> {
    Ok(Self {
      md5: Context::try_new(&MD5_FOR_LEGACY_USE_ONLY).map_err(|_| DriveRefusal::DigestFailed)?,
      sha1: Context::try_new(&SHA1_FOR_LEGACY_USE_ONLY).map_err(|_| DriveRefusal::DigestFailed)?,
    })
  }

  /// Takes `bytes` in.
  fn update(&mut self, bytes: &[u8]) -> Result<(), DriveRefusal> {
    self
      .md5
      .try_update(bytes)
      .map_err(|_| DriveRefusal::DigestFailed)?;
    self
      .sha1
      .try_update(bytes)
      .map_err(|_| DriveRefusal::DigestFailed)
  }

  /// The two digests.
  fn finish(self) -> Result<([u8; 16], [u8; 20]), DriveRefusal> {
    let md5 = self
      .md5
      .try_finish()
      .map_err(|_| DriveRefusal::DigestFailed)?;
    let sha1 = self
      .sha1
      .try_finish()
      .map_err(|_| DriveRefusal::DigestFailed)?;
    Ok((
      <[u8; 16]>::try_from(md5.as_ref()).map_err(|_| DriveRefusal::DigestFailed)?,
      <[u8; 20]>::try_from(sha1.as_ref()).map_err(|_| DriveRefusal::DigestFailed)?,
    ))
  }
}

/// The MD5 of `bytes`.
fn md5_of(bytes: &[u8]) -> Result<[u8; 16], DriveRefusal> {
  let mut digests = Digests::new()?;
  digests.update(bytes)?;
  Ok(digests.finish()?.0)
}

/// The lock tokens a commit presents, read as `add_lock_tokens` reads them (`serve.c` 1360-1409): each path joined to
/// the session's, which the principal must be named and allowed to write. Returns the commit's locks, or `None` when
/// they were refused, the failure written and the command over.
fn presented_locks(
  request: &CommitRequest<'_, '_>,
  session: &Session<'_>,
  out: &mut Vec<u8>,
) -> Result<Option<CommitLocks>, ItemError> {
  let tokens = match request.lock_tokens() {
    Ok(tokens) => tokens,
    Err(refusal) => {
      write_failure(&DriveRefusal::LockTokens(refusal), out)?;
      return Ok(None);
    }
  };
  let mut presented = BTreeMap::new();
  for (path, token) in tokens {
    let (Ok(path), Ok(token)) = (std::str::from_utf8(path), std::str::from_utf8(token)) else {
      write_failure(
        &DriveRefusal::LockTokens(LockTokensRefusal::PathNotString),
        out,
      )?;
      return Ok(None);
    };
    let full = joined(session.base_path, &relative(path));
    if !may_write(session, Some(&full), true) {
      write_failure_words(SVN_ERR_RA_NOT_AUTHORIZED, NOT_AUTHORIZED, out)?;
      return Ok(None);
    }
    presented.insert(full, token.to_owned());
  }
  Ok(Some(CommitLocks {
    user: session.user.map(str::to_owned),
    presented,
    keep: request.keep_locks(),
  }))
}

/// `lookup_access` for a write (`serve.c` 760-791): the session's level from `svnserve.conf`, then the rules, at
/// `path` or, for none, anywhere in the repository (`svn_repos_authz_check_access` with no path), then a name when
/// one is needed.
fn may_write(session: &Session<'_>, path: Option<&str>, needs_username: bool) -> bool {
  let level = if session.user.is_some() {
    session.authenticated
  } else {
    session.anonymous
  };
  let ruled = session.authz.is_none_or(|(authz, name)| {
    let rules = authz.rules(name, Principal::named(session.user.map(str::as_bytes)));
    let granted = match path {
      None => rules.access_anywhere(),
      Some(path) => {
        RepositoryPath::new(path.as_bytes()).map_or(Access::None, |at| rules.access(at))
      }
    };
    granted == Access::Write
  });
  level == Access::Write && ruled && (!needs_username || session.user.is_some())
}

/// The revision properties a commit records but its date (`serve.c` 1526-1541): the client's, or its log message
/// alone when it sent none; `svn:author` always the principal, never the client's word, and absent for none.
pub fn revision_properties(
  request: &CommitRequest<'_, '_>,
  user: Option<&str>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
  let mut properties: BTreeMap<Vec<u8>, Vec<u8>> = match request
    .rest
    .as_ref()
    .and_then(|rest| rest.revision_properties.as_ref())
  {
    Some(sent) => sent
      .iter()
      .map(|(name, value)| (name.to_vec(), value.to_vec()))
      .collect(),
    None => BTreeMap::from([(REVISION_LOG.to_vec(), request.log_message.to_vec())]),
  };
  properties.remove(REVISION_AUTHOR);
  if let Some(user) = user {
    properties.insert(REVISION_AUTHOR.to_vec(), user.as_bytes().to_vec());
  }
  properties.into_iter().collect()
}

/// Writes `svnserve`'s answer to an accepted commit once the caller committed it (`editorp.c` 905, `serve.c`
/// 1562-1576): `close-edit`'s `( success ( ) )`, the trivial authentication request, then the new revision, its date,
/// its author and post-commit's words if its hook failed.
///
/// # Errors
/// [`ItemError`] when an answer cannot be written, which none here can give.
pub fn write_commit_info(
  revision: Revision,
  date: &str,
  author: Option<&str>,
  post_commit_error: Option<&str>,
  out: &mut Vec<u8>,
) -> Result<(), ItemError> {
  write(
    &Value::List(vec![Value::Word(SUCCESS), Value::List(Vec::new())]),
    out,
  )?;
  write(&trivial_auth(), out)?;
  write(
    &Value::List(vec![
      Value::Number(revision),
      optional_string(Some(date)),
      optional_string(author),
      optional_string(post_commit_error),
    ]),
    out,
  )
}

/// An optional C string as `"(?c)"` writes it: a list of it, or an empty list.
fn optional_string(text: Option<&str>) -> Value<'_> {
  Value::List(
    text
      .map(|text| Value::String(text.as_bytes()))
      .into_iter()
      .collect(),
  )
}

/// `trivial_auth_request`: `( success ( ( ) 0: ) )`.
fn trivial_auth() -> Value<'static> {
  Value::List(vec![
    Value::Word(SUCCESS),
    Value::List(vec![Value::List(Vec::new()), Value::String(b"")]),
  ])
}

/// Writes a refusal that needs nothing of a drive's, before the drive begins.
fn write_failure(refusal: &DriveRefusal, out: &mut Vec<u8>) -> Result<(), ItemError> {
  write_failure_words(refusal.subversion_code(), &default_message(refusal), out)
}

/// The words of the commit rule's refusals for a commit by `user` (`commit.c` 164-198; `tree.c` `conflict_err`;
/// `lock.c` `verify_lock` 605-630): what the drive writes at `close-edit`, and what the owner's step writes for a work
/// it judged again where its verdict's facts moved (A-655 part 2b; [`super::revision::StepRefusal::Verdict`]).
pub fn commit_words(refusal: &CommitRefusal, user: Option<&str>) -> String {
  match refusal {
    CommitRefusal::OutOfDate { path, kind } => match kind {
      Some(NodeKind::Directory) => format!("Directory '{path}' is out of date"),
      Some(NodeKind::File) => format!("File '{path}' is out of date"),
      None => format!("'{path}' is out of date"),
    },
    CommitRefusal::RevisionPastBase { revision } => format!("No such revision {revision}"),
    CommitRefusal::Conflict { path, .. } => format!("{CONFLICT_AT} '{path}'"),
    CommitRefusal::Locked(refusal) => lock_words(refusal, user),
    CommitRefusal::Index(refusal) => format!("{refusal:?}"),
  }
}

/// FSFS's words for a lock in the way of a commit by `user` (`lock.c` `verify_lock` 605-630).
fn lock_words(refusal: &LockRefusal, user: Option<&str>) -> String {
  match refusal {
    LockRefusal::NoUser { path: Some(path) } => {
      format!("Cannot verify lock on path '{path}'; no username available")
    }
    LockRefusal::OwnerMismatch { path, owner } => format!(
      "User '{}' does not own lock on path '{path}' (currently locked by '{owner}')",
      user.unwrap_or_default()
    ),
    LockRefusal::TokenNotPresented { path } => {
      format!("Cannot verify lock on path '{path}'; no matching lock-token available")
    }
    other => format!("{other:?}"),
  }
}

/// The words of a refusal that needs nothing of the drive's (`serve.c` `add_lock_tokens` 1374-1395).
fn default_message(refusal: &DriveRefusal) -> String {
  match refusal {
    DriveRefusal::LockTokens(LockTokensRefusal::NotListOfLists) => {
      "Lock tokens aren't a list of lists".to_owned()
    }
    DriveRefusal::LockTokens(LockTokensRefusal::TokenNotString | LockTokensRefusal::ShortPair) => {
      "Lock token isn't a string".to_owned()
    }
    DriveRefusal::LockTokens(_) => "Lock path isn't a string".to_owned(),
    other => format!("{other:?}"),
  }
}

/// A relpath as `svn_relpath_canonicalize` leaves it: no empty or `.` segment.
fn relative(path: &str) -> String {
  path
    .split(SEPARATOR)
    .filter(|segment| !segment.is_empty() && *segment != CURRENT)
    .collect::<Vec<_>>()
    .join("/")
}

/// `relative` joined to `base` (`svn_fspath__join`).
fn joined(base: &str, relative: &str) -> String {
  if relative.is_empty() {
    base.to_owned()
  } else if base == ROOT {
    format!("{ROOT}{relative}")
  } else {
    format!("{base}{SEPARATOR}{relative}")
  }
}

/// The value of one hexadecimal digit.
fn hex_digit(byte: u8) -> Option<u8> {
  char::from(byte)
    .to_digit(u32::from(HEX))
    .and_then(|digit| u8::try_from(digit).ok())
}

/// A byte from its two hexadecimal digits.
fn hex_byte(high: u8, low: u8) -> Option<u8> {
  hex_digit(high)?
    .checked_mul(HEX)?
    .checked_add(hex_digit(low)?)
}

/// A URL percent-decoded as `svn_path_uri_decode` decodes it: an escape that is not two hexadecimal digits is kept.
fn uri_decoded(url: &[u8]) -> Vec<u8> {
  let mut decoded = Vec::with_capacity(url.len());
  let mut rest = url;
  while let [byte, after @ ..] = rest {
    let escaped = match rest {
      [PERCENT, high, low, ..] => hex_byte(*high, *low),
      _ => None,
    };
    match escaped {
      Some(value) => {
        decoded.push(value);
        rest = rest.get(ESCAPE_BYTES..).unwrap_or_default();
      }
      None => {
        decoded.push(*byte);
        rest = after;
      }
    }
  }
  decoded
}

/// A checksum read as `svn_checksum_parse_hex` reads an MD5: its first 32 hexadecimal digits, the rest ignored; `None`
/// for the all-zero digest, which Subversion reads as no checksum at all and so matches any text.
fn parsed_md5(checksum: &[u8]) -> Result<Option<[u8; 16]>, DriveRefusal> {
  let mut digest = [0_u8; MD5_BYTES];
  let mut pairs = checksum.chunks(2);
  for byte in &mut digest {
    let Some([high, low]) = pairs.next() else {
      return Err(DriveRefusal::ChecksumSyntax);
    };
    *byte = hex_byte(*high, *low).ok_or(DriveRefusal::ChecksumSyntax)?;
  }
  Ok(digest.iter().any(|byte| *byte != 0).then_some(digest))
}

/// A digest as `svn_checksum_to_cstring_display` writes it: lower-case hexadecimal.
fn displayed(digest: &[u8]) -> String {
  digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `svn_checksum_mismatch_err`'s words after its description (`checksum.c` 688-694).
fn mismatch(description: &str, expected: &[u8], actual: &[u8]) -> String {
  format!(
    "{description}:\n   expected:  {}\n     actual:  {}\n",
    displayed(expected),
    displayed(actual)
  )
}

/// `svndiff.c`'s words for a refused delta (`svndiff.c` 495-539, 658-959; `compress_lz4.c` 117-122;
/// `text_delta.c` 785). Of the three refusals of a version-2 section's length, which the codec reads as one, the
/// last's words are written.
fn delta_message(refusal: SvndiffError) -> String {
  match refusal {
    SvndiffError::InvalidHeader | SvndiffError::VersionNotOffered => {
      "Svndiff has invalid header".to_owned()
    }
    SvndiffError::WindowTooLarge => "Svndiff contains a too-large window".to_owned(),
    SvndiffError::CorruptWindowHeader => "Svndiff contains corrupt window header".to_owned(),
    SvndiffError::BackwardSourceView => "Svndiff has backwards-sliding source views".to_owned(),
    SvndiffError::InvalidInstruction { index, fault } => instruction_message(index, fault),
    SvndiffError::TargetNotFilled => "Delta does not fill the target window".to_owned(),
    SvndiffError::NewDataUnused => "Delta does not contain enough new data".to_owned(),
    SvndiffError::UnexpectedEnd => "Unexpected end of svndiff input".to_owned(),
    SvndiffError::InvalidCompressedData => {
      "Size of uncompressed data does not match stored original length".to_owned()
    }
    SvndiffError::Lz4 => "LZ4 decompression failed".to_owned(),
    SvndiffError::SourceShort => "Delta source ended unexpectedly".to_owned(),
  }
}

/// `svndiff.c`'s words for a refused instruction (495-526).
fn instruction_message(index: usize, fault: InstructionFault) -> String {
  let what = match fault {
    InstructionFault::Undecodable => "insn {} cannot be decoded",
    InstructionFault::ZeroLength => "insn {} has length zero",
    InstructionFault::PastTarget => "insn {} overflows the target view",
    InstructionFault::PastSource => "[src] insn {} overflows the source view",
    InstructionFault::TargetNotBehind => "[tgt] insn {} starts beyond the target view position",
    InstructionFault::PastNewData => "[new] insn {} overflows the new data section",
  };
  format!(
    "Invalid diff stream: {}",
    what.replacen("{}", &index.to_string(), 1)
  )
}

/// A node as the transaction holds it when a command arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct View {
  /// Its kind.
  kind: NodeKind,
  /// Its created revision; `None` once the drive changed it or anything beneath it.
  created: Option<Revision>,
  /// Its text.
  text: Option<Text>,
  /// Its property list.
  properties: PropertyList,
}

impl View {
  /// The view of a node the index holds.
  fn of(node: &crate::index::Node<'_>) -> Self {
    Self {
      kind: node.kind(),
      created: Some(node.created_revision()),
      text: node.text(),
      properties: node.properties(),
    }
  }
}

/// One commit's drive (module documentation).
pub struct Drive {
  /// What it runs with.
  setup: Setup,
  /// Where it is.
  phase: Phase,
  /// Whether `open-root` came and its start-commit passed.
  opened: bool,
  /// What `open-root` makes once its start-commit passes.
  pending_root: Option<PendingRoot>,
  /// The transaction's base: the youngest at `open-root`.
  base: Revision,
  /// The tokens the client named, by their bytes.
  tokens: BTreeMap<Vec<u8>, Baton>,
  /// The changed-path rows, one per path, folded as FSFS folds them.
  rows: BTreeMap<String, ChangedPath>,
  /// The texts the work made.
  texts: BTreeMap<ContentIdentity, Vec<u8>>,
  /// The property lists the work made.
  property_lists: BTreeMap<ContentIdentity, Vec<(Vec<u8>, Vec<u8>)>>,
  /// The bytes held: the work's texts and property lists, and its open deltas'.
  held: usize,
  /// The kinds of the nodes the drive deleted and then replaced, by path, for a deletion of the replacement.
  replaced: BTreeMap<String, NodeKind>,
}

impl Drive {
  /// A drive about to read its first editor command.
  pub fn new(setup: Setup) -> Self {
    Self {
      setup,
      phase: Phase::Driving,
      opened: false,
      pending_root: None,
      base: 0,
      tokens: BTreeMap::new(),
      rows: BTreeMap::new(),
      texts: BTreeMap::new(),
      property_lists: BTreeMap::new(),
      held: 0,
      replaced: BTreeMap::new(),
    }
  }

  /// Takes one item the client sent during the drive, writing what `svnserve` writes for it. An item handed in while a
  /// hook is due ([`Step::Hook`]) ends the drive ([`Step::Close`]): `svnserve` reads nothing while its hook runs.
  ///
  /// # Errors
  /// [`ItemError`] when an answer cannot be written, which none here can give.
  pub fn command<G: Green + ?Sized>(
    &mut self,
    index: &Index,
    green: &G,
    locks: &LockTable,
    item: &Value<'_>,
    out: &mut Vec<u8>,
  ) -> Result<Step, ItemError> {
    match self.phase {
      Phase::Done => return Ok(Step::Close),
      Phase::Waiting(_) => {
        self.phase = Phase::Done;
        return Ok(Step::Close);
      }
      Phase::Dropping => return Ok(self.drop_one(item)),
      Phase::Driving => {}
    }
    let command = match EditCommand::read(item) {
      Ok(command) => command,
      Err(EditRefusal::Malformed(_)) => return Ok(Step::Close),
      Err(EditRefusal::UnknownCommand { name }) => {
        return self.refuse(&DriveRefusal::UnknownCommand { name }, out);
      }
    };
    let world = World {
      index,
      green,
      locks,
    };
    let outcome = self.run(&world, &command);
    self.answer(outcome, out)
  }

  /// Takes the answer of the hook the last command waits for (A-661): `Ok` goes on with the command, `open-root`'s
  /// transaction made and the command taken, `close-edit`'s verdict judged against `index` and `locks` as they stand
  /// now; `Err` holds the errors `svnserve` writes for the hook's refusal, outermost first, and refuses the command
  /// with them under the early-error rule (written, then every item dropped until `abort-edit`). Called with no hook
  /// due, the drive ends ([`Step::Close`]).
  ///
  /// # Errors
  /// [`ItemError`] when an answer cannot be written, which none here can give.
  pub fn resume<G: Green + ?Sized>(
    &mut self,
    index: &Index,
    green: &G,
    locks: &LockTable,
    answer: Result<(), Vec<(u32, String)>>,
    out: &mut Vec<u8>,
  ) -> Result<Step, ItemError> {
    let Phase::Waiting(pause) = self.phase else {
      self.phase = Phase::Done;
      return Ok(Step::Close);
    };
    self.phase = Phase::Driving;
    if let Err(errors) = answer {
      write_failure_errors(&errors, out)?;
      self.phase = Phase::Dropping;
      return Ok(Step::Refused);
    }
    let outcome = match pause {
      Pause::OpenRoot => self.make_root(),
      Pause::CloseEdit => self.judge(&World {
        index,
        green,
        locks,
      }),
    };
    self.answer(outcome, out)
  }

  /// The transaction as a hook reads it while its command waits ([`Step::Hook`]): its name, base, revision properties,
  /// rows, texts and property lists so far.
  pub fn pending(&self) -> Pending<'_> {
    Pending {
      transaction: &self.setup.transaction,
      base: self.base,
      revision_properties: &self.setup.revision_properties,
      rows: &self.rows,
      texts: &self.texts,
      property_lists: &self.property_lists,
    }
  }

  /// What a command's outcome makes of the drive, and its step.
  fn answer(
    &mut self,
    outcome: Result<Outcome, Stop>,
    out: &mut Vec<u8>,
  ) -> Result<Step, ItemError> {
    match outcome {
      Ok(Outcome::Taken) => Ok(Step::Taken),
      Ok(Outcome::Aborted) => {
        self.phase = Phase::Done;
        write(
          &Value::List(vec![Value::Word(SUCCESS), Value::List(Vec::new())]),
          out,
        )?;
        Ok(Step::Ended)
      }
      Ok(Outcome::Accepted(work)) => {
        self.phase = Phase::Done;
        Ok(Step::Accepted(work))
      }
      Ok(Outcome::Hook(due)) => {
        self.phase = Phase::Waiting(match due {
          Due::StartCommit { .. } => Pause::OpenRoot,
          Due::PreCommit { .. } => Pause::CloseEdit,
        });
        Ok(Step::Hook(due))
      }
      Err(Stop::Close) => {
        self.phase = Phase::Done;
        Ok(Step::Close)
      }
      Err(Stop::Refused(refusal)) => self.refuse(&refusal, out),
    }
  }

  /// The words `svnserve` writes for `refusal` in this drive.
  pub fn message(&self, refusal: &DriveRefusal) -> String {
    match refusal {
      DriveRefusal::Commit(refusal) => commit_words(refusal, self.setup.locks.user.as_deref()),
      DriveRefusal::PastHead { revision, youngest } => {
        format!("No such revision {revision} (HEAD is {youngest})")
      }
      DriveRefusal::NotPresent { path } => format!("Path '{path}' not present"),
      DriveRefusal::NotFound { path } => {
        format!(
          "File not found: transaction '{}', path '{path}'",
          self.setup.transaction
        )
      }
      DriveRefusal::SourceNotFound { path, revision } => {
        format!("File not found: revision {revision}, path '{path}'")
      }
      DriveRefusal::SourceRevision { revision } => format!("No such revision {revision}"),
      DriveRefusal::AlreadyExists { path } => format!(
        "File already exists: filesystem '{}', transaction '{}', path '{path}'",
        self.setup.filesystem, self.setup.transaction
      ),
      DriveRefusal::ForeignSource { url } => {
        format!("Source url '{url}' is from different repository")
      }
      DriveRefusal::BaseChecksum {
        path,
        expected,
        actual,
      } => mismatch(
        &format!("Base checksum mismatch on '{path}'"),
        expected,
        actual,
      ),
      DriveRefusal::ResultChecksum {
        path,
        expected,
        actual,
      } => mismatch(
        &format!("Checksum mismatch for resulting fulltext\n({path})"),
        expected,
        actual,
      ),
      DriveRefusal::ChecksumSyntax => "Invalid character in hex checksum".to_owned(),
      DriveRefusal::Delta(refusal) => delta_message(*refusal),
      DriveRefusal::NoTransaction => "No valid transaction supplied to close_edit".to_owned(),
      DriveRefusal::UnknownCommand { name } => format!("Unknown editor command '{name}'"),
      DriveRefusal::ControlCharacter { byte, path } => format!(
        "Invalid control character '0x{byte:02x}' in path '{}'",
        escaped(path)
      ),
      DriveRefusal::NotRegular { name } => format!(
        "Storage of non-regular property '{name}' is disallowed through the repository interface, and could \
         indicate a bug in your client"
      ),
      DriveRefusal::NotUtf8 { name } => {
        format!("Cannot accept '{name}' property because it is not encoded in UTF-8")
      }
      DriveRefusal::NonLf { .. } => "Invalid property value".to_owned(),
      DriveRefusal::BadDate => "Wrong or unexpected property value".to_owned(),
      DriveRefusal::MergeinfoTerminator { path } => format!(
        "Commit rejected because mergeinfo on '{path}' contains unexpected string terminator"
      ),
      DriveRefusal::MergeinfoInvalid { path, .. } => {
        format!("Commit rejected because mergeinfo on '{path}' is syntactically invalid")
      }
      DriveRefusal::Unwritable | DriveRefusal::Unreadable => "Access denied".to_owned(),
      DriveRefusal::Room { .. } => {
        "The client request size exceeds the configured limit".to_owned()
      }
      other => default_message(other),
    }
  }

  /// The errors `svnserve` writes for `refusal`, outermost first: one, or a chain where Subversion wraps an error in
  /// another (`marshal.c` `svn_ra_svn__write_cmd_failure` writes each).
  pub fn errors(&self, refusal: &DriveRefusal) -> Vec<(u32, String)> {
    match refusal {
      DriveRefusal::NonLf { name } => vec![
        (SVN_ERR_BAD_PROPERTY_VALUE, self.message(refusal)),
        (
          SVN_ERR_BAD_PROPERTY_VALUE_EOL,
          format!("Cannot accept non-LF line endings in '{name}' property"),
        ),
      ],
      DriveRefusal::BadDate => vec![
        (SVN_ERR_BAD_PROPERTY_VALUE, self.message(refusal)),
        (SVN_ERR_BAD_DATE, "Bogus date".to_owned()),
      ],
      other => vec![(other.subversion_code(), self.message(other))],
    }
  }

  /// Writes a refusal's failure and turns to dropping (`editorp.c` 1149-1174).
  fn refuse(&mut self, refusal: &DriveRefusal, out: &mut Vec<u8>) -> Result<Step, ItemError> {
    write_failure_errors(&self.errors(refusal), out)?;
    self.phase = Phase::Dropping;
    Ok(Step::Refused)
  }

  /// One item after a refusal: `abort-edit` or `success` ends the drive, anything else is dropped unread
  /// (`editorp.c` 1193-1208).
  fn drop_one(&mut self, item: &Value<'_>) -> Step {
    let ends = match item {
      Value::List(parts) => {
        matches!(parts.first(), Some(Value::Word(word)) if *word == ABORT_EDIT || *word == SUCCESS)
      }
      _ => false,
    };
    if ends {
      self.phase = Phase::Done;
      Step::Ended
    } else {
      Step::Dropped
    }
  }

  /// Runs one command.
  fn run<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    command: &EditCommand<'_>,
  ) -> Result<Outcome, Stop> {
    match *command {
      EditCommand::TargetRev { .. } => Ok(Outcome::Taken),
      EditCommand::OpenRoot { revision, root } => self.open_root(world.index, revision, root),
      EditCommand::DeleteEntry {
        path,
        revision,
        directory,
      } => self.delete_entry(world, path, revision, directory),
      EditCommand::AddDir {
        path,
        parent,
        child,
        copied_from,
      } => self.add(
        world,
        Added {
          path,
          parent,
          child,
          copied_from,
          kind: NodeKind::Directory,
        },
      ),
      EditCommand::AddFile {
        path,
        directory,
        file,
        copied_from,
      } => self.add(
        world,
        Added {
          path,
          parent: directory,
          child: file,
          copied_from,
          kind: NodeKind::File,
        },
      ),
      EditCommand::OpenDir {
        path,
        parent,
        child,
        revision,
      } => self.open_dir(world, path, parent, child, revision),
      EditCommand::ChangeDirProp {
        directory,
        name,
        value,
      } => self.change_dir_prop(world, directory, name, value),
      EditCommand::CloseDir { directory } => self.close_dir(directory),
      EditCommand::AbsentDir { parent, .. } | EditCommand::AbsentFile { parent, .. } => {
        self.directory(parent).map(|_| Outcome::Taken)
      }
      EditCommand::OpenFile {
        path,
        directory,
        file,
        revision,
      } => self.open_file(world, path, directory, file, revision),
      EditCommand::ApplyTextdelta {
        file,
        base_checksum,
      } => self.apply_textdelta(world, file, base_checksum),
      EditCommand::TextdeltaChunk { file, chunk } => self.textdelta_chunk(world.green, file, chunk),
      EditCommand::TextdeltaEnd { file } => self.textdelta_end(world.green, file),
      EditCommand::ChangeFileProp { file, name, value } => {
        let path = self.file(file)?.path.clone();
        self.check_written(file, &path)?;
        self.change_property(world, &path, name, value)
      }
      EditCommand::CloseFile {
        file,
        text_checksum,
      } => self.close_file(world, file, text_checksum),
      EditCommand::CloseEdit => self.close_edit(),
      EditCommand::AbortEdit => Ok(Outcome::Aborted),
      EditCommand::FinishReplay | EditCommand::Failure => Err(Stop::Close),
    }
  }

  /// `open-root` (`commit.c` 413-466): the revision checked, then the transaction begun on the youngest as
  /// `svn_repos_fs_begin_txn_for_commit2` begins it (`fs-wrap.c` 132-184): its revision properties checked
  /// (`svn_repos_fs_change_txn_props`, 344-357), then start-commit ([`Due::StartCommit`]); the root is opened once
  /// the hook passed ([`Drive::resume`]).
  fn open_root(
    &mut self,
    index: &Index,
    revision: Option<u64>,
    root: &[u8],
  ) -> Result<Outcome, Stop> {
    let youngest = index.youngest();
    if let Some(revision) = revision.filter(|revision| *revision > youngest) {
      return Err(DriveRefusal::PastHead { revision, youngest }.into());
    }
    for (name, value) in &self.setup.revision_properties {
      validate_prop(name, Some(value.as_slice()))?;
    }
    self.base = youngest;
    self.pending_root = Some(PendingRoot {
      token: root.to_vec(),
      revision,
    });
    Ok(Outcome::Hook(Due::StartCommit {
      transaction: self.setup.transaction.clone(),
    }))
  }

  /// The root opened once start-commit passed: the transaction made on the youngest `open-root` read.
  fn make_root(&mut self) -> Result<Outcome, Stop> {
    let PendingRoot { token, revision } = self.pending_root.take().ok_or(Stop::Close)?;
    self.opened = true;
    self.rows.clear();
    let directory = Directory {
      path: self.setup.base_path.clone(),
      copied: false,
      base: revision,
      checked: false,
    };
    self.tokens.insert(token, Baton::Directory(directory));
    Ok(Outcome::Taken)
  }

  /// `delete-entry` (`commit.c` 470-516; `tree.c` `fs_delete_node`).
  fn delete_entry<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    path: &[u8],
    revision: Option<u64>,
    directory: &[u8],
  ) -> Result<Outcome, Stop> {
    let parent = self.directory(directory)?.path.clone();
    let full = self.full_path(path)?;
    let node = self.view(world.index, &full)?;
    let recursive = node.is_some_and(|node| node.kind == NodeKind::Directory);
    self.check_authz(&full, true, recursive)?;
    self.check_authz(&parent, true, false)?;
    let Some(node) = node else {
      return Err(
        CommitRefusal::OutOfDate {
          path: full,
          kind: None,
        }
        .into(),
      );
    };
    if let Some(revision) = revision {
      check_out_of_date(&full, node.kind, revision, node.created, self.base)?;
    }
    self.check_lock(world.locks, &full, true)?;
    self.delete(&full, node.kind);
    Ok(Outcome::Taken)
  }

  /// `add-dir` and `add-file` (`commit.c` 296-407; `tree.c` `fs_make_dir`, `fs_make_file`, `copy_helper`).
  fn add<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    added: Added<'_>,
  ) -> Result<Outcome, Stop> {
    let parent = self.directory(added.parent)?;
    let (copied, parent) = (parent.copied, parent.path.clone());
    if added.kind == NodeKind::File {
      self.tokens.insert(
        added.child.to_vec(),
        Baton::File(File {
          path: String::new(),
          delta: None,
          checked: false,
        }),
      );
    }
    validate_new_path(added.path)?;
    let full = self.full_path(added.path)?;
    let directory = added.kind == NodeKind::Directory;
    self.check_authz(&full, true, directory && added.copied_from.is_some())?;
    self.check_authz(&parent, true, false)?;
    let present = self.view(world.index, &full)?;
    let copy_from = match added.copied_from {
      Some(source) => {
        Some(self.copy_source(world, &full, present.as_ref(), copied, (source, added.kind))?)
      }
      None => {
        if present.is_some() {
          return Err(DriveRefusal::AlreadyExists { path: full }.into());
        }
        self.check_lock(world.locks, &full, added.kind == NodeKind::Directory)?;
        None
      }
    };
    self.place(&full, added.kind, copy_from.clone(), present.is_some());
    let baton = match added.kind {
      NodeKind::Directory => Baton::Directory(Directory {
        path: full,
        copied: copied || copy_from.is_some(),
        base: None,
        checked: true,
      }),
      NodeKind::File => Baton::File(File {
        path: full,
        delta: None,
        checked: true,
      }),
    };
    self.tokens.insert(added.child.to_vec(), baton);
    Ok(Outcome::Taken)
  }

  /// A copy's checks (`commit.c` 323-366; `tree.c` `copy_helper`): the target absent unless its directory was copied,
  /// the source in this repository and present at its revision, then the target's locks.
  fn copy_source<G: Green + ?Sized>(
    &self,
    world: &World<'_, G>,
    full: &str,
    present: Option<&View>,
    copied: bool,
    (source, kind): (CopiedFrom<'_>, NodeKind),
  ) -> Result<CopySource, Stop> {
    if let Some(node) = present
      && !copied
    {
      return Err(
        CommitRefusal::OutOfDate {
          path: full.to_owned(),
          kind: Some(node.kind),
        }
        .into(),
      );
    }
    let path = self.source_path(source.path)?;
    if source.revision > world.index.youngest() {
      return Err(
        DriveRefusal::SourceRevision {
          revision: source.revision,
        }
        .into(),
      );
    }
    self.check_authz(&path, false, kind == NodeKind::Directory)?;
    if world.index.node(&path, source.revision)?.is_none() {
      return Err(
        DriveRefusal::SourceNotFound {
          path,
          revision: source.revision,
        }
        .into(),
      );
    }
    self.check_lock(world.locks, full, true)?;
    Ok(CopySource {
      path,
      revision: source.revision,
    })
  }

  /// A copy source's path in the repository: its URL decoded, less the repository's root URL (`commit.c` 346-353).
  fn source_path(&self, url: &[u8]) -> Result<String, Stop> {
    let decoded = uri_decoded(url);
    let Ok(decoded) = String::from_utf8(decoded) else {
      return Err(
        DriveRefusal::PathSyntax {
          path: String::from_utf8_lossy(url).into_owned(),
        }
        .into(),
      );
    };
    let foreign = || {
      Stop::Refused(DriveRefusal::ForeignSource {
        url: decoded.clone(),
      })
    };
    let rest = decoded
      .strip_prefix(self.setup.repository_url.as_str())
      .ok_or_else(foreign)?;
    if rest.is_empty() {
      return Ok(ROOT.to_owned());
    }
    let inner = rest.strip_prefix(SEPARATOR).ok_or_else(foreign)?;
    Ok(joined(ROOT, &relative(inner)))
  }

  /// `open-dir` (`commit.c` 532-560).
  fn open_dir<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    path: &[u8],
    parent: &[u8],
    child: &[u8],
    revision: Option<u64>,
  ) -> Result<Outcome, Stop> {
    let copied = self.directory(parent)?.copied;
    let full = self.full_path(path)?;
    if self.view(world.index, &full)?.is_none() {
      let path = relative(&String::from_utf8_lossy(path));
      return Err(DriveRefusal::NotPresent { path }.into());
    }
    let directory = Directory {
      path: full,
      copied,
      base: revision,
      checked: false,
    };
    self
      .tokens
      .insert(child.to_vec(), Baton::Directory(directory));
    Ok(Outcome::Taken)
  }

  /// `change-dir-prop` (`commit.c` 700-732; `tree.c` `fs_change_node_prop`): the out-of-date test at the first change,
  /// then the locks at or beneath.
  fn change_dir_prop<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    token: &[u8],
    name: &[u8],
    value: Option<&[u8]>,
  ) -> Result<Outcome, Stop> {
    let directory = self.directory(token)?.clone();
    if !directory.checked {
      self.check_authz(&directory.path, true, false)?;
      if let Some(base) = directory.base {
        let node = self.view(world.index, &directory.path)?;
        let node = node.ok_or_else(|| DriveRefusal::NotFound {
          path: directory.path.clone(),
        })?;
        check_out_of_date(
          &directory.path,
          NodeKind::Directory,
          base,
          node.created,
          self.base,
        )?;
      }
      if let Some(Baton::Directory(directory)) = self.tokens.get_mut(token) {
        directory.checked = true;
      }
    }
    self.change_property(world, &directory.path, name, value)
  }

  /// `close-dir`: its token let go (`editorp.c` 661-678).
  fn close_dir(&mut self, token: &[u8]) -> Result<Outcome, Stop> {
    self.directory(token)?;
    self.tokens.remove(token);
    Ok(Outcome::Taken)
  }

  /// `open-file` (`commit.c` 604-647): the file present, then the out-of-date test.
  fn open_file<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    path: &[u8],
    directory: &[u8],
    file: &[u8],
    revision: Option<u64>,
  ) -> Result<Outcome, Stop> {
    self.directory(directory)?;
    self.tokens.insert(
      file.to_vec(),
      Baton::File(File {
        path: String::new(),
        delta: None,
        checked: false,
      }),
    );
    let full = self.full_path(path)?;
    self.check_authz(&full, false, false)?;
    let Some(node) = self.view(world.index, &full)? else {
      return Err(DriveRefusal::NotFound { path: full }.into());
    };
    if let Some(revision) = revision {
      check_out_of_date(&full, NodeKind::File, revision, node.created, self.base)?;
    }
    self.tokens.insert(
      file.to_vec(),
      Baton::File(File {
        path: full,
        delta: None,
        checked: false,
      }),
    );
    Ok(Outcome::Taken)
  }

  /// `apply-textdelta` (`editorp.c` 762-785; `tree.c` `fs_apply_textdelta`): the file's own lock, then the base
  /// checksum; the delta opened on the file's text.
  fn apply_textdelta<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    token: &[u8],
    base_checksum: Option<&[u8]>,
  ) -> Result<Outcome, Stop> {
    let path = match self.file(token)? {
      File { delta: Some(_), .. } => return Err(Stop::Close),
      File { path, .. } => path.clone(),
    };
    self.check_written(token, &path)?;
    let expected = base_checksum.map(parsed_md5).transpose()?.flatten();
    let Some(node) = self.view(world.index, &path)? else {
      return Err(DriveRefusal::NotFound { path }.into());
    };
    self.check_lock(world.locks, &path, false)?;
    if let (Some(expected), Some(text)) = (expected, node.text)
      && expected != text.md5
    {
      return Err(
        DriveRefusal::BaseChecksum {
          path,
          expected,
          actual: text.md5,
        }
        .into(),
      );
    }
    let delta = Delta {
      decoder: Decoder::new(),
      pending: Vec::new(),
      source: node.text.map(|text| text.identity),
      target: Vec::new(),
      digests: Digests::new()?,
    };
    if let Some(Baton::File(file)) = self.tokens.get_mut(token) {
      file.delta = Some(delta);
    }
    Ok(Outcome::Taken)
  }

  /// `textdelta-chunk` (`editorp.c` 788-805): the chunk read on, every whole window applied to the file's text.
  fn textdelta_chunk<G: Green + ?Sized>(
    &mut self,
    green: &G,
    token: &[u8],
    chunk: &[u8],
  ) -> Result<Outcome, Stop> {
    let held = self.held;
    let room = self.setup.room;
    if held.saturating_add(chunk.len()) > room {
      return Err(DriveRefusal::Room { room }.into());
    }
    let texts = &self.texts;
    let Some(Baton::File(File {
      delta: Some(delta), ..
    })) = self.tokens.get_mut(token)
    else {
      return Err(Stop::Close);
    };
    let before = delta.pending.len().saturating_add(delta.target.len());
    delta.pending.extend_from_slice(chunk);
    let source = match delta.source {
      Some(identity) => texts
        .get(&identity)
        .map(Vec::as_slice)
        .or_else(|| green.text(&identity)),
      None => Some(&[][..]),
    };
    let source = source.ok_or(DriveRefusal::Commit(CommitRefusal::Index(
      IndexRefusal::Corrupt,
    )))?;
    read_windows(delta, source)?;
    let after = delta.pending.len().saturating_add(delta.target.len());
    let held = held.saturating_sub(before).saturating_add(after);
    if held > room {
      return Err(DriveRefusal::Room { room }.into());
    }
    self.held = held;
    Ok(Outcome::Taken)
  }

  /// `textdelta-end` (`editorp.c` 808-826): the delta must end whole; the new text named, kept and declared.
  fn textdelta_end<G: Green + ?Sized>(&mut self, green: &G, token: &[u8]) -> Result<Outcome, Stop> {
    let Some(Baton::File(file)) = self.tokens.get_mut(token) else {
      return Err(Stop::Close);
    };
    let Some(delta) = file.delta.take() else {
      return Err(Stop::Close);
    };
    let path = file.path.clone();
    delta.decoder.finish(&delta.pending)?;
    let (md5, sha1) = delta.digests.finish()?;
    let size = u64::try_from(delta.target.len()).map_err(|_| DriveRefusal::Room {
      room: self.setup.room,
    })?;
    let identity = green.name_text(&delta.target);
    let text = Text {
      identity,
      md5,
      sha1,
      size,
    };
    self.texts.insert(identity, delta.target);
    self.record(&path, NodeKind::File, |row| row.text = Some(text));
    Ok(Outcome::Taken)
  }

  /// `close-file` (`commit.c` 672-697): the resulting text's MD5 against the client's.
  fn close_file<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    token: &[u8],
    text_checksum: Option<&[u8]>,
  ) -> Result<Outcome, Stop> {
    let path = self.file(token)?.path.clone();
    if let Some(checksum) = text_checksum {
      let node = self
        .view(world.index, &path)?
        .ok_or_else(|| DriveRefusal::NotFound { path: path.clone() })?;
      let actual = match node.text {
        Some(text) => text.md5,
        None => md5_of(&[])?,
      };
      if let Some(expected) = parsed_md5(checksum)?
        && expected != actual
      {
        return Err(
          DriveRefusal::ResultChecksum {
            path,
            expected,
            actual,
          }
          .into(),
        );
      }
    }
    self.tokens.remove(token);
    Ok(Outcome::Taken)
  }

  /// `close-edit` (`commit.c` 798-882): with no transaction refused (811-813), else pre-commit first
  /// ([`Due::PreCommit`]), as `svn_repos_fs_commit_txn` runs it before FSFS's merge; the verdict follows once the hook
  /// passed ([`Drive::resume`]).
  fn close_edit(&mut self) -> Result<Outcome, Stop> {
    if !self.opened {
      return Err(DriveRefusal::NoTransaction.into());
    }
    Ok(Outcome::Hook(Due::PreCommit {
      transaction: self.setup.transaction.clone(),
    }))
  }

  /// `close-edit` once pre-commit passed: the verdict against the head as it stands now (A-592's: FSFS's merge, then
  /// the lock table's check), and the accepted work without the ephemeral `svn:txn-` properties, which
  /// `svn_repos_fs_commit_txn` drops after pre-commit and before it commits (`fs-wrap.c` 72-89).
  fn judge<G: Green + ?Sized>(&mut self, world: &World<'_, G>) -> Result<Outcome, Stop> {
    let record = RevisionRecord {
      transaction: self.setup.transaction.clone(),
      changes: self.rows.values().cloned().collect(),
    };
    let commit = &self.setup.locks;
    let now = self.setup.now;
    let lock_entries = verdict(world.index, self.base, &record, |changes| {
      world.locks.verdict(
        changes
          .iter()
          .map(|change| (change.path.as_str(), change.action)),
        commit,
        now,
      )
    })?;
    let mut revision_properties = std::mem::take(&mut self.setup.revision_properties);
    revision_properties.retain(|(name, _)| !name.starts_with(TXN_PREFIX));
    Ok(Outcome::Accepted(Box::new(Work {
      head: world.index.youngest(),
      record,
      lock_entries,
      revision_properties,
      texts: std::mem::take(&mut self.texts),
      property_lists: std::mem::take(&mut self.property_lists),
      locks: self.setup.locks.clone(),
    })))
  }

  /// A property set or deleted on the node at `path`, its locks met first (`tree.c` `fs_change_node_prop`).
  fn change_property<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    path: &str,
    name: &[u8],
    value: Option<&[u8]>,
  ) -> Result<Outcome, Stop> {
    validate_property(path, name, value)?;
    let node = self
      .view(world.index, path)?
      .ok_or_else(|| DriveRefusal::NotFound {
        path: path.to_owned(),
      })?;
    self.check_lock(world.locks, path, node.kind == NodeKind::Directory)?;
    let mut list = self.property_list(world.green, node.properties)?;
    list.retain(|(held, _)| held.as_slice() != name);
    if let Some(value) = value {
      list.push((name.to_vec(), value.to_vec()));
      list.sort();
    }
    let properties = self.held_list(world.green, list)?;
    let mergeinfo_modified = name == MERGEINFO;
    self.record(path, node.kind, |row| {
      row.properties = Some(properties);
      row.mergeinfo_modified |= mergeinfo_modified;
    });
    Ok(Outcome::Taken)
  }

  /// The property list held as `properties`, from the work's own or the green's.
  fn property_list<G: Green + ?Sized>(
    &self,
    green: &G,
    properties: PropertyList,
  ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, DriveRefusal> {
    let PropertyList::Held { identity, .. } = properties else {
      return Ok(Vec::new());
    };
    if let Some(list) = self.property_lists.get(&identity) {
      return Ok(list.clone());
    }
    let list = green.properties(&identity).ok_or(IndexRefusal::Corrupt)?;
    let mut list: Vec<(Vec<u8>, Vec<u8>)> = list
      .into_iter()
      .map(|(name, value)| (name.to_vec(), value.to_vec()))
      .collect();
    list.sort();
    Ok(list)
  }

  /// A new property list named by the green and kept, within the room.
  fn held_list<G: Green + ?Sized>(
    &mut self,
    green: &G,
    list: Vec<(Vec<u8>, Vec<u8>)>,
  ) -> Result<PropertyList, DriveRefusal> {
    if list.is_empty() {
      return Ok(PropertyList::Empty);
    }
    let identity = green.name_properties(&list);
    let has_mergeinfo = list.iter().any(|(name, _)| name.as_slice() == MERGEINFO);
    let bytes = list.iter().fold(0_usize, |sum, (name, value)| {
      sum.saturating_add(name.len()).saturating_add(value.len())
    });
    let held = self.held.saturating_add(bytes);
    if held > self.setup.room {
      return Err(DriveRefusal::Room {
        room: self.setup.room,
      });
    }
    self.held = held;
    self.property_lists.insert(identity, list);
    Ok(PropertyList::Held {
      identity,
      has_mergeinfo,
    })
  }

  /// A path deleted, folded into the rows as FSFS folds its changes (module documentation): what the drive added
  /// there and beneath is gone with it, and a replacement becomes the deletion of what it replaced, whose kind the
  /// drive kept.
  fn delete(&mut self, path: &str, kind: NodeKind) {
    let beneath = format!("{path}{SEPARATOR}");
    let prior = self.rows.remove(path);
    self.rows.retain(|row, _| !row.starts_with(&beneath));
    let deleted_kind = match prior {
      Some(ChangedPath {
        action: Action::Add,
        ..
      }) => None,
      Some(ChangedPath {
        action: Action::Replace,
        ..
      }) => self.replaced.remove(path).or(Some(kind)),
      _ => Some(kind),
    };
    if let Some(kind) = deleted_kind {
      self.rows.insert(path.to_owned(), deletion(path, kind));
    }
  }

  /// A node or a copy placed at `path`, folded into the rows: an addition, or a replacement of what the drive deleted
  /// there or of what a copied directory brought.
  fn place(&mut self, path: &str, kind: NodeKind, copy_from: Option<CopySource>, replaces: bool) {
    let deleted = match self.rows.get(path) {
      Some(row) if row.action == Action::Delete => Some(row.kind),
      _ => None,
    };
    if let Some(replaced) = deleted {
      self.replaced.insert(path.to_owned(), replaced);
    }
    let action = if replaces || deleted.is_some() {
      Action::Replace
    } else {
      Action::Add
    };
    self.rows.insert(
      path.to_owned(),
      ChangedPath {
        path: path.to_owned(),
        action,
        kind,
        copy_from,
        text: None,
        properties: None,
        mergeinfo_modified: false,
      },
    );
  }

  /// Sets what a row carries, making it a modification when the drive had none at `path`.
  fn record(&mut self, path: &str, kind: NodeKind, set: impl FnOnce(&mut ChangedPath)) {
    let row = self
      .rows
      .entry(path.to_owned())
      .or_insert_with(|| ChangedPath {
        path: path.to_owned(),
        action: Action::Modify,
        kind,
        copy_from: None,
        text: None,
        properties: None,
        mergeinfo_modified: false,
      });
    set(row);
  }

  /// `check_authz` (`commit.c` 243-262) by A-606's rules: read, or write, at `path`, and beneath it when `recursive`.
  fn check_authz(&self, path: &str, write: bool, recursive: bool) -> Result<(), DriveRefusal> {
    let Some(rules) = self.setup.rules.as_ref() else {
      return Ok(());
    };
    let at = RepositoryPath::new(path.as_bytes()).map_err(|_| DriveRefusal::PathSyntax {
      path: path.to_owned(),
    })?;
    let granted = if recursive {
      rules.access_beneath(at)
    } else {
      rules.access(at)
    };
    match (write, granted) {
      (true, Access::Write) | (false, Access::Read | Access::Write) => Ok(()),
      (true, _) => Err(DriveRefusal::Unwritable),
      (false, _) => Err(DriveRefusal::Unreadable),
    }
  }

  /// A file's first change: its write checked once (`checked_write`, `commit.c` 573-579 and 659-665).
  fn check_written(&mut self, token: &[u8], path: &str) -> Result<(), DriveRefusal> {
    if matches!(
      self.tokens.get(token),
      Some(Baton::File(File { checked: true, .. }))
    ) {
      return Ok(());
    }
    self.check_authz(path, true, false)?;
    if let Some(Baton::File(file)) = self.tokens.get_mut(token) {
      file.checked = true;
    }
    Ok(())
  }

  /// A path's locks met as FSFS meets them at a call (`svn_fs_fs__allow_locked_operation`): at or beneath it when
  /// `recursive`, its own otherwise.
  fn check_lock(&self, locks: &LockTable, path: &str, recursive: bool) -> Result<(), DriveRefusal> {
    let action = if recursive {
      Action::Delete
    } else {
      Action::Modify
    };
    let commit = CommitLocks {
      keep: true,
      ..self.setup.locks.clone()
    };
    locks.verdict([(path, action)], &commit, self.setup.now)?;
    Ok(())
  }

  /// The transaction's node at `path`, as its rows leave it over the index at the base (module documentation).
  fn view(&self, index: &Index, path: &str) -> Result<Option<View>, DriveRefusal> {
    let changed_beneath = self.changed_beneath(path);
    let Some(row) = self.rows.get(path) else {
      let found = self.beneath_rows(index, path)?;
      return Ok(found.map(|view| View {
        created: if changed_beneath { None } else { view.created },
        ..view
      }));
    };
    let under = match (row.action, &row.copy_from) {
      (Action::Delete, _) => return Ok(None),
      (Action::Add | Action::Replace, None) => None,
      (Action::Add | Action::Replace, Some(source)) => index
        .node(&source.path, source.revision)?
        .map(|node| View::of(&node)),
      (Action::Modify, _) => self.beneath_rows(index, path)?,
    };
    let kind = under.map_or(row.kind, |under| under.kind);
    Ok(Some(View {
      kind,
      created: None,
      text: row.text.or_else(|| under.and_then(|under| under.text)),
      properties: row
        .properties
        .or_else(|| under.map(|under| under.properties))
        .unwrap_or(PropertyList::Empty),
    }))
  }

  /// The node at `path` that the rows of the directories above it leave, the path's own row aside: hidden by the
  /// nearest deleted or fresh directory, read through the nearest copied one, else the base's.
  fn beneath_rows(&self, index: &Index, path: &str) -> Result<Option<View>, DriveRefusal> {
    let mut ancestor = path;
    while let Some(end) = ancestor.rfind(SEPARATOR) {
      ancestor = if end == 0 {
        ROOT
      } else {
        ancestor.get(..end).unwrap_or(ROOT)
      };
      if let Some(row) = self.rows.get(ancestor) {
        match (row.action, &row.copy_from) {
          (Action::Delete, _) | (Action::Add | Action::Replace, None) => return Ok(None),
          (Action::Add | Action::Replace, Some(source)) => {
            let rest = path.get(ancestor.len()..).unwrap_or_default();
            let inside = joined(&source.path, rest.trim_start_matches(SEPARATOR));
            return Ok(
              index
                .node(&inside, source.revision)?
                .map(|node| View::of(&node)),
            );
          }
          (Action::Modify, _) => {}
        }
      }
      if ancestor == ROOT {
        break;
      }
    }
    Ok(index.node(path, self.base)?.map(|node| View::of(&node)))
  }

  /// Whether the drive changed anything at or beneath `path`, which FSFS has made mutable.
  fn changed_beneath(&self, path: &str) -> bool {
    if path == ROOT {
      return !self.rows.is_empty();
    }
    let beneath = format!("{path}{SEPARATOR}");
    self
      .rows
      .range(beneath.clone()..)
      .next()
      .is_some_and(|(row, _)| row.starts_with(&beneath))
  }

  /// An editor path joined to the session's (`svn_relpath_canonicalize`, `svn_fspath__join`).
  fn full_path(&self, path: &[u8]) -> Result<String, DriveRefusal> {
    let Ok(path) = std::str::from_utf8(path) else {
      return Err(DriveRefusal::PathSyntax {
        path: String::from_utf8_lossy(path).into_owned(),
      });
    };
    Ok(joined(&self.setup.base_path, &relative(path)))
  }

  /// A directory token's directory, or the connection's end (`editorp.c` `lookup_token`).
  fn directory(&self, token: &[u8]) -> Result<&Directory, Stop> {
    match self.tokens.get(token) {
      Some(Baton::Directory(directory)) => Ok(directory),
      _ => Err(Stop::Close),
    }
  }

  /// A file token's file, or the connection's end (`editorp.c` `lookup_token`).
  fn file(&self, token: &[u8]) -> Result<&File, Stop> {
    match self.tokens.get(token) {
      Some(Baton::File(file)) => Ok(file),
      _ => Err(Stop::Close),
    }
  }
}

/// An `add-dir` or `add-file`'s parameters.
struct Added<'a> {
  /// The path, relative to the session.
  path: &'a [u8],
  /// The directory's token.
  parent: &'a [u8],
  /// The new token.
  child: &'a [u8],
  /// The copy's source.
  copied_from: Option<CopiedFrom<'a>>,
  /// The node's kind.
  kind: NodeKind,
}

/// What a command reads besides the drive: the index, the green and the lock table, each the owner's as the command
/// arrives.
struct World<'w, G: ?Sized> {
  /// The index.
  index: &'w Index,
  /// The green.
  green: &'w G,
  /// The lock table.
  locks: &'w LockTable,
}

/// What a command came to.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
  /// Taken.
  Taken,
  /// `abort-edit`.
  Aborted,
  /// `close-edit`, accepted.
  Accepted(Box<Work>),
  /// Waiting for a hook.
  Hook(Due),
}

/// A deletion's row.
fn deletion(path: &str, kind: NodeKind) -> ChangedPath {
  ChangedPath {
    path: path.to_owned(),
    action: Action::Delete,
    kind,
    copy_from: None,
    text: None,
    properties: None,
    mergeinfo_modified: false,
  }
}

/// Reads every whole part of a delta's pending bytes, applying each window to `source` and taking the new bytes into
/// the digests.
fn read_windows(delta: &mut Delta, source: &[u8]) -> Result<(), DriveRefusal> {
  let mut consumed_total = 0_usize;
  loop {
    let rest = delta.pending.get(consumed_total..).unwrap_or_default();
    match delta.decoder.decode(rest)? {
      Decoded::Header { consumed, .. } => consumed_total = consumed_total.saturating_add(consumed),
      Decoded::Window { window, consumed } => {
        let offset =
          usize::try_from(window.source_offset).map_err(|_| SvndiffError::SourceShort)?;
        let view = source.get(offset..).unwrap_or_default();
        let start = delta.target.len();
        svndiff::apply(&window, view, &mut delta.target)?;
        delta
          .digests
          .update(delta.target.get(start..).unwrap_or_default())?;
        consumed_total = consumed_total.saturating_add(consumed);
      }
      Decoded::NeedMore { .. } => break,
    }
  }
  delta
    .pending
    .drain(..consumed_total.min(delta.pending.len()));
  Ok(())
}

/// `svn_repos__validate_new_path` (`repos.c` 2097; `path.c` `svn_path_check_valid`): an added path holds no control
/// character.
fn validate_new_path(path: &[u8]) -> Result<(), DriveRefusal> {
  match path.iter().find(|byte| byte.is_ascii_control()) {
    Some(&byte) => Err(DriveRefusal::ControlCharacter {
      byte,
      path: path.to_vec(),
    }),
    None => Ok(()),
  }
}

/// A path as `svn_path_illegal_path_escape` writes it in a message: each control character as `\` and three octal
/// digits (`path.c`).
fn escaped(path: &[u8]) -> String {
  let mut out = Vec::with_capacity(path.len());
  for &byte in path {
    if byte.is_ascii_control() {
      out.extend_from_slice(format!("\\{byte:03o}").as_bytes());
    } else {
      out.push(byte);
    }
  }
  String::from_utf8_lossy(&out).into_owned()
}

/// `svn_repos_fs_change_node_prop`'s checks before FSFS's (`fs-wrap.c` 327-338): `svn:mergeinfo` verified, then
/// `svn_repos__validate_prop` (202-263). A deletion is never refused.
fn validate_property(path: &str, name: &[u8], value: Option<&[u8]>) -> Result<(), DriveRefusal> {
  if let Some(value) = value
    && name == MERGEINFO
  {
    verify_mergeinfo(path, value)?;
  }
  validate_prop(name, value)
}

/// `svn_repos__validate_prop` (`fs-wrap.c` 202-263), which a node's property change meets and so does every revision
/// property `open-root` begins the transaction with (`svn_repos_fs_change_txn_props`, 344-357): a deletion never
/// refused; a property `svn:wc:` or `svn:entry:` never stored; an `svn:` value UTF-8 without a carriage return, and
/// `svn:date` a date.
fn validate_prop(name: &[u8], value: Option<&[u8]>) -> Result<(), DriveRefusal> {
  let Some(value) = value else {
    return Ok(());
  };
  let named = || String::from_utf8_lossy(name).into_owned();
  if name.starts_with(WC_PREFIX) || name.starts_with(ENTRY_PREFIX) {
    return Err(DriveRefusal::NotRegular { name: named() });
  }
  if !name.starts_with(SVN_PREFIX) {
    return Ok(());
  }
  if std::str::from_utf8(value).is_err() {
    return Err(DriveRefusal::NotUtf8 { name: named() });
  }
  let c_string = value.split(|byte| *byte == NUL).next().unwrap_or_default();
  if c_string.contains(&CARRIAGE_RETURN) {
    return Err(DriveRefusal::NonLf { name: named() });
  }
  if name == REVISION_DATE && !is_date(c_string) {
    return Err(DriveRefusal::BadDate);
  }
  Ok(())
}

/// `verify_mergeinfo` (`fs-wrap.c` 297-324): no NUL, then A-605's parser.
fn verify_mergeinfo(path: &str, value: &[u8]) -> Result<(), DriveRefusal> {
  if value.contains(&NUL) {
    return Err(DriveRefusal::MergeinfoTerminator {
      path: path.to_owned(),
    });
  }
  Mergeinfo::parse(value)
    .map(|_| ())
    .map_err(|refusal| DriveRefusal::MergeinfoInvalid {
      path: path.to_owned(),
      refusal,
    })
}

/// Whether `text` reads as a date the way `svn_time_from_cstring` reads one (`time.c`): `YYYY-MM-DDTHH:MM:SS.uuuuuuZ`,
/// each number `svn__strtoul`'s digits, then APR's check (`apr_time_exp_gmt_get`): a month from 1 to 12, and a time
/// not before 1970. The format `svn` stopped writing in 2002 is refused (module documentation).
fn is_date(text: &[u8]) -> bool {
  let mut fields = [0_i64; DATE_FIELDS];
  let mut rest = text;
  for (field, separator) in fields.iter_mut().zip(DATE_SEPARATORS) {
    let (value, after) = strtoul(rest);
    let Some((&next, after)) = after.split_first() else {
      return false;
    };
    if next != separator {
      return false;
    }
    *field = value;
    rest = after;
  }
  let [year, month, day, hour, minute, second, _] = fields;
  epoch_seconds([year, month, day], [hour, minute, second]).is_some_and(|seconds| seconds >= 0)
}

/// `svn__strtoul`'s leading decimal digits, wrapping as C's unsigned long does, then cast to `apr_int32_t` as `time.c`
/// casts them.
fn strtoul(text: &[u8]) -> (i64, &[u8]) {
  let digits = text.iter().take_while(|byte| byte.is_ascii_digit()).count();
  let (number, rest) = text.split_at_checked(digits).unwrap_or((text, &[]));
  let value = number.iter().fold(0_u64, |value, digit| {
    value
      .wrapping_mul(DECIMAL)
      .wrapping_add(u64::from(digit.wrapping_sub(b'0')))
  });
  let low = u32::try_from(value & u64::from(u32::MAX)).unwrap_or_default();
  (i64::from(i32::from_ne_bytes(low.to_ne_bytes())), rest)
}

/// The seconds since 1970 `apr_time_exp_get` computes for a date, or `None` for a month outside the year.
fn epoch_seconds([year, month, day]: [i64; 3], [hour, minute, second]: [i64; 3]) -> Option<i64> {
  let month = month.checked_sub(1)?;
  let offset = *DAY_OFFSET.get(usize::try_from(month).ok()?)?;
  let mut year = year.checked_sub(TM_YEAR_BASE)?;
  if month < 2 {
    year = year.checked_sub(1)?;
  }
  let centuries = year.checked_div(CENTURY)?;
  let days = year
    .checked_mul(DAYS_PER_YEAR)?
    .checked_add(year.checked_div(LEAP_EVERY)?)?
    .checked_sub(centuries)?
    .checked_add(
      centuries
        .checked_add(CENTURY_LEAP_OFFSET)?
        .checked_div(LEAP_EVERY)?,
    )?
    .checked_add(offset)?
    .checked_add(day)?
    .checked_sub(1)?
    .checked_sub(EPOCH_DAYS)?;
  days
    .checked_mul(HOURS_PER_DAY)?
    .checked_add(hour)?
    .checked_mul(SIXTY)?
    .checked_add(minute)?
    .checked_mul(SIXTY)?
    .checked_add(second)
}

/// Writes a failure with every error of its chain, outermost first, each with a release build's empty file and zero
/// line.
fn write_failure_errors(errors: &[(u32, String)], out: &mut Vec<u8>) -> Result<(), ItemError> {
  let items = errors.iter().map(|(code, message)| {
    Value::List(vec![
      Value::Number(u64::from(*code)),
      Value::String(message.as_bytes()),
      Value::String(b""),
      Value::Number(0),
    ])
  });
  write(
    &Value::List(vec![Value::Word(FAILURE), Value::List(items.collect())]),
    out,
  )
}

/// Writes a failure of one error.
fn write_failure_words(code: u32, message: &str, out: &mut Vec<u8>) -> Result<(), ItemError> {
  let error = Value::List(vec![
    Value::Number(u64::from(code)),
    Value::String(message.as_bytes()),
    Value::String(b""),
    Value::Number(0),
  ]);
  write(
    &Value::List(vec![Value::Word(FAILURE), Value::List(vec![error])]),
    out,
  )
}

#[cfg(test)]
mod tests;
