//! The ra_svn read commands (§4.17.4 "4. Reads" and "5. The wire", piece 7's second part; A-613): a sans-io handler
//! that answers one parsed command of a session after its handshake, from the Subversion index ([`crate::index`],
//! A-553), the mergeinfo queries ([`crate::mergeinfo`], A-605) and the green's revision properties and property lists
//! ([`Repository`]), writing exactly what `svnserve` writes.
//!
//! Every answer is `serve.c`'s (Subversion `1.14.x` at `d434dc14`; the deployed binaries stock 1.15.0), held to
//! transcripts of stock `svnserve` answering stock `svn` and scripted clients (`crates/svn/fixtures/reads`, each file's
//! SHA-256 in `SHA256SUMS`):
//! - **The order of a command's writes.** A command that runs at all writes the trivial authentication request first,
//!   `( success ( ( ) 0: ) )` (`trivial_auth_request`, the protocol's §2 "auth-request" with no mechanism), then its
//!   answer or its refusal; so a revision past the youngest is refused after it. A command refused read access writes
//!   its refusal alone (below).
//! - **Parameters as `svn_ra_svn__parse_tuple` reads them.** Missing or of the wrong kind is malformed data, which
//!   `svnserve` answers by ending the connection without a word ([`Outcome::Close`]); parameters past the prototype
//!   are ignored; a string read as a C string ends at its first NUL; a revision is the number cast to a signed long, so
//!   a number past 2^63 - 1 is negative and refused as an invalid revision.
//! - **A refusal is `( failure ( ( code message 0: 0 ) ) )`**, Subversion's error number, its message as the C code
//!   formats it, and the empty file and zero line of a release build; the connection goes on.
//! - **A command no piece answers** is refused at once, no authentication request first: "Unknown editor command"
//!   (`SVN_ERR_RA_SVN_UNKNOWN_CMD`, `marshal.c` `handle_command`). A command of the main set another piece answers
//!   (commit, update, locks, the rest of piece 7) is handed back untouched ([`Outcome::Elsewhere`]).
//! - **Paths.** A command's path is the session's base path joined with the given relative path made canonical
//!   (`svn_relpath_canonicalize_safe`, `svn_fspath__join`): empty and `.` names dropped, `..` kept, so it names nothing.
//! - **Read access** (A-606; `serve.c` `must_have_access`, `lookup_access`, `authz_check_access`). A session carries
//!   its principal's rules when the repository names an authorization file ([`Session::authz`]); without one every path
//!   is readable. `get-file`, `get-dir`, `check-path`, `stat`, `get-iprops` and `list` check their path first:
//!   readable, the trivial authentication request; not, the refusal "Authorization failed" (170001) and nothing before
//!   it. Then each command passes over or stops at what the session may not read, as `libsvn_repos` does: a directory's
//!   and a listing's hidden entries, hidden ancestors' properties and hidden requested mergeinfo paths are left out;
//!   revision properties show as much as the revision is readable (all, the author and the date, or none:
//!   `svn_repos_check_revision_access`); `get-locations`, `get-location-segments` and `log -g`'s history refuse a
//!   hidden path 220001 "Unreadable path encountered; access denied" and end at a hidden copy source; `log` refuses a
//!   hidden path or root 220001 "Item is not readable", ends a path's history at a hidden location and censors each
//!   revision as `detect_changed` finds it; `get-file-revs` ends at a hidden location, and ends the connection with
//!   nothing written for the command where `svnserve`'s assertion aborts its process (its authentication request still
//!   in the unflushed buffer, as the recording shows). `get-latest-rev`, `get-dated-rev` and `get-deleted-rev` check
//!   nothing. Where the session is anonymous and the client could authenticate for read access
//!   ([`Authentication::Available`]), a path check that fails asks it to instead of refusing, as `must_have_access`
//!   does: nothing is written ([`Outcome::Authenticate`]), the connection asks, and the command runs again with the
//!   principal the client proved ([`Authentication::Done`]), its answer with no trivial request before it, or refused
//!   "Authorization failed" where that principal may not read the path either.
//! - **Property lists in name order.** `svnserve` writes a property list in its hash table's order, which APR seeds per
//!   table (`apr_hash_make` since APR 1.5), so three recordings of one repository gave three orders; a client reads the
//!   list into a hash. The handler writes them in byte order of their names, and the tests compare them as sets.
//! - **Dates** are read as `svn_time_from_cstring` reads them (fields by `svn__strtoul`, each cast to 32 bits, then
//!   APR's `apr_time_exp_gmt_get`: a month outside 1-12 or a time before 1970 refused, every other field unchecked) and
//!   written as `svn_time_to_cstring` writes them. The pre-1.0 timestamp format `svn_time_from_cstring` still accepts is
//!   refused as a bogus date: no client sends it and no repository stores it.
//!
//! The commands answered here: `get-latest-rev`, `get-dated-rev` (`svn_repos_dated_revision`'s binary search),
//! `rev-proplist`, `rev-prop`, `check-path`, `stat` (`svn_repos_stat`), `get-mergeinfo`
//! (`svn_repos_fs_get_mergeinfo2`), `get-iprops` (`svn_repos_fs_get_inherited_props`), `get-dir`, `get-locations`
//! (`svn_repos_trace_node_locations`), `get-file`, `list`, `get-location-segments`, `get-deleted-rev`, `log` with its
//! merged revisions and `get-file-revs` (each its own module, `file`, `list`, `segments`, `deleted`, `log` and
//! `file_revs`, over FSFS's node history in `history`).
//! (`svn mergeinfo --show-revs` is the stock client's own work over `get-mergeinfo`, `log` and the location segments,
//! never a server command.)
//!
//! Bounds: an answer holds what its command names, bounded by the index and the green (a property list, a revision's
//! properties, a request's paths and their mergeinfo). A command whose answer grows with the repository writes its
//! head and hands back a [`Stream`] that writes the rest a slice at a time, in the room the caller gives each slice
//! (module `stream`): a file's text is never held whole, nor a listing.
//!
//! No I/O, no clock, no randomness, no panic.

use crate::authz::{PrincipalRules, may_read};
use crate::handshake::ClientCapabilities;
use crate::index::{
  ContentIdentity, Index, IndexRefusal, Node, NodeKind, PropertyList, PropertyPairs, Revision,
};
use crate::lock::{AprTime, LockTable};
use crate::mergeinfo::{self, Contents, MergeinfoInheritance, QueryRefusal};
use crate::ra_svn::{ItemError, Value, write};
use crate::report::drive::history as copies;

/// `SVN_ERR_BAD_DATE`, "Bogus date".
/// Format: the bad-input category's start, 120,000 + 1 × 5,000 = 125,000, plus 3 (`svn_error_codes.h` 107-108, 177-178).
const SVN_ERR_BAD_DATE: u32 = 125_003;

/// `SVN_ERR_FS_GENERAL`.
/// Format: the filesystem category's start, 120,000 + 8 × 5,000 = 160,000, plus 0 (`svn_error_codes.h` 121-122).
const SVN_ERR_FS_GENERAL: u32 = 160_000;

/// `SVN_ERR_FS_CORRUPT`.
/// Format: the filesystem category's start plus 4 (`svn_error_codes.h` 597-598).
const SVN_ERR_FS_CORRUPT: u32 = 160_004;

/// `SVN_ERR_FS_NO_SUCH_REVISION`.
/// Format: the filesystem category's start plus 6 (`svn_error_codes.h`).
const SVN_ERR_FS_NO_SUCH_REVISION: u32 = 160_006;

/// `SVN_ERR_FS_NOT_FOUND`.
/// Format: the filesystem category's start plus 13 (`svn_error_codes.h` 633-634).
const SVN_ERR_FS_NOT_FOUND: u32 = 160_013;

/// `SVN_ERR_RA_SVN_UNKNOWN_CMD`.
/// Format: the ra_svn category's start, 120,000 + 18 × 5,000 = 210,000, plus 1 (`svn_error_codes.h` 141-142).
const SVN_ERR_RA_SVN_UNKNOWN_CMD: u32 = 210_001;

/// The word of a successful response.
/// Format: `command-response: ( success params:list )` (`protocol` §3).
const SUCCESS: &str = "success";

/// The word of a refusal.
/// Format: `( failure ( err:error ... ) )` (`protocol` §3).
const FAILURE: &str = "failure";

/// The boolean true.
/// Format: `bool: true|false` (`protocol` §3.1).
const TRUE_WORD: &str = "true";

/// The boolean false.
/// Format: `bool: true|false` (`protocol` §3.1).
const FALSE_WORD: &str = "false";

/// The kind of a path that holds nothing.
/// Format: `node-kind: none|file|dir|unknown` (`protocol` §3.1); `svn_node_kind_to_word`.
const NONE_WORD: &str = "none";

/// The kind of a file.
/// Format: `node-kind` (`protocol` §3.1).
const FILE_WORD: &str = "file";

/// The kind of a directory.
/// Format: `node-kind` (`protocol` §3.1).
const DIR_WORD: &str = "dir";

/// The size a directory's dirent carries: Subversion's invalid file size, `(svn_filesize_t) -1`, written unsigned.
/// Format: `stat_cmd` writes `(apr_uint64_t) dirent->size` (`serve.c`), a directory's `SVN_INVALID_FILESIZE`; recorded
/// as 18446744073709551615 (fixture `stat`).
const DIRECTORY_SIZE: u64 = u64::MAX;

/// The revision property holding a revision's date.
/// Format: `SVN_PROP_REVISION_DATE` (`svn_props.h`).
const DATE_PROPERTY: &[u8] = b"svn:date";

/// The entry properties `get_props` adds to a node's own (`svn_props.h` `SVN_PROP_ENTRY_*`).
/// Format: `svn:entry:committed-rev`, `svn:entry:committed-date`, `svn:entry:last-author`, `svn:entry:uuid`.
const ENTRY_COMMITTED_REV: &[u8] = b"svn:entry:committed-rev";

/// The entry property of a node's committed date.
/// Format: `SVN_PROP_ENTRY_COMMITTED_DATE`.
const ENTRY_COMMITTED_DATE: &[u8] = b"svn:entry:committed-date";

/// The entry property of a node's last author.
/// Format: `SVN_PROP_ENTRY_LAST_AUTHOR`.
const ENTRY_LAST_AUTHOR: &[u8] = b"svn:entry:last-author";

/// The entry property of the repository's UUID.
/// Format: `SVN_PROP_ENTRY_UUID`.
const ENTRY_UUID: &[u8] = b"svn:entry:uuid";

/// The dirent fields a `get-dir` asks for, by their words (`protocol` §3.1.1 `dirent-field`).
/// Format: `parse_dirent_fields` (`serve.c`): `kind`, `size`, `has-props`, `created-rev`, `time`, `last-author`; any
/// other word is ignored, and no list asks for all.
const DIRENT_FIELDS: [&str; 6] = [
  "kind",
  "size",
  "has-props",
  "created-rev",
  "time",
  "last-author",
];

/// The word `get-locations` ends its entries with.
/// Format: `location-entry: ( rev:number abs-path:number ) | done` (`protocol` §3.1.1).
const DONE: &str = "done";

/// SVN_ERR_FS_NOT_DIRECTORY, "Can't get entries of non-directory".
/// Format: the filesystem category's start plus 16 (`svn_error_codes.h`); recorded (fixture `get-dir`).
const SVN_ERR_FS_NOT_DIRECTORY: u32 = 160_016;

/// The revision property holding a revision's author.
/// Format: `SVN_PROP_REVISION_AUTHOR` (`svn_props.h`).
const AUTHOR_PROPERTY: &[u8] = b"svn:author";

/// The root's path.
/// Format: Subversion's canonical absolute path of the root (`svn_fspath__canonicalize`).
const ROOT: &str = "/";

/// The byte between a path's names.
/// Format: Subversion's path separator (`dirent_uri.c`).
const SEPARATOR: char = '/';

/// The name meaning the directory itself, which canonicalization drops.
/// Format: "Empty or noop segment, so do nothing" (`dirent_uri.c` `canonicalize`).
const CURRENT_DIRECTORY: &str = ".";

/// The byte a C string ends at.
/// Format: `svn_ra_svn__parse_tuple`'s `c` hands the string's data to C code, which reads it to its first NUL.
const TERMINATOR: u8 = 0;

/// The commands of `svnserve`'s main command set another piece answers.
/// Format: `serve.c` `main_commands`, less the commands answered here.
const ELSEWHERE: [&str; 16] = [
  "reparent",
  "change-rev-prop",
  "change-rev-prop2",
  "commit",
  "update",
  "switch",
  "status",
  "diff",
  "lock",
  "lock-many",
  "unlock",
  "unlock-many",
  "get-lock",
  "get-locks",
  "replay",
  "replay-range",
];

/// The separators of `svn_time_from_cstring`'s format, each after its field: year, month, day, hour, minute, second,
/// microsecond.
/// Format: `TIMESTAMP_FORMAT`, `%04d-%02d-%02dT%02d:%02d:%02d.%06dZ` (`time.c`).
const TIME_SEPARATORS: [u8; 7] = *b"--T::.Z";

/// The days from the 1st of March to the 1st of each month, January and February counted in the year before.
/// Format: APR's `apr_time_exp_get` `dayoffset` (`time/unix/time.c`), which starts a year on the 1st of March so that a
/// leap day ends it.
const DAY_OFFSET: [i64; 12] = [306, 337, 0, 31, 61, 92, 122, 153, 184, 214, 245, 275];

/// The days from the 1st of March 1900 to the 1st of January 1970.
/// Format: APR's `apr_time_exp_get`, "1 jan 1970 is 25508 days since 1 mar 1900".
const EPOCH_FROM_1900_MARCH: i64 = 25_508;

/// The months January and February, which `apr_time_exp_get` counts in the year before.
/// Format: `if (xt->tm_mon < 2) year--;` (`apr_time_exp_get`).
const MARCH: i64 = 2;

/// February's month number, the last month `civil_from_days` counts in the year after its era's year.
/// Format: `civil_from_days`: `y + (m <= 2)`.
const FEBRUARY: i64 = 2;

/// The year C's `struct tm` counts from.
/// Format: C's `struct tm`, `tm_year` years since 1900.
const TM_YEAR_BASE: i32 = 1900;

/// Days in a common year.
/// Format: the Gregorian calendar.
const YEAR_DAYS: i64 = 365;

/// The years of the leap cycle, the century and the four centuries.
/// Format: the Gregorian rule, as `apr_time_exp_get` writes it: `year/4 - year/100 + (year/100 + 3)/4`.
const LEAP_YEARS: i64 = 4;

/// The years of a century.
/// Format: the Gregorian rule.
const CENTURY: i64 = 100;

/// The centuries' offset in `apr_time_exp_get`'s four-century term, `(year/100 + 3)/4`.
/// Format: `apr_time_exp_get` (`time/unix/time.c`).
const CENTURY_OFFSET: i64 = 3;

/// Microseconds in a second.
/// Format: `APR_USEC_PER_SEC`.
const MICROSECONDS: i64 = 1_000_000;

/// Seconds in a minute and minutes in an hour.
/// Format: the clock.
const SIXTY: i64 = 60;

/// Hours in a day.
/// Format: the clock.
const HOURS: i64 = 24;

/// Seconds in a day.
/// Format: the clock, 24 × 60 × 60.
const DAY_SECONDS: i64 = 86_400;

/// The days from 0000-03-01 to 1970-01-01 in the proleptic Gregorian calendar, which `civil_from_days` counts from.
/// Format: Howard Hinnant, "chrono-Compatible Low-Level Date Algorithms", `civil_from_days` (the algorithm `gmtime`
/// computes); 719,468 = 1970 × 365 + 1970/4 - 1970/100 + 1970/400 - 306 + 1, the days of the 1,970 years, less March
/// to December of year 0.
const EPOCH_FROM_YEAR_ZERO_MARCH: i64 = 719_468;

/// The days of a 400-year era of the Gregorian calendar.
/// Format: 400 × 365 + 97 leap days.
const ERA_DAYS: i64 = 146_097;

/// The years of an era.
/// Format: the Gregorian cycle.
const ERA_YEARS: i64 = 400;

/// The last day of the first four years, century and era of an era counted from the 1st of March: the corrections of
/// `civil_from_days`' year of the era.
/// Format: `civil_from_days`: `(doe - doe/1460 + doe/36524 - doe/146096) / 365`.
const ERA_CORRECTIONS: [i64; 3] = [1_460, 36_524, 146_096];

/// The days of the five months March to July, by which `civil_from_days` finds a month from a day of the year.
/// Format: `civil_from_days`: `mp = (5*doy + 2)/153`, `d = doy - (153*mp+2)/5 + 1`; 31 + 30 + 31 + 30 + 31 = 153.
const FIVE_MONTH_DAYS: i64 = 153;

/// The months of that cycle and its rounding.
/// Format: `civil_from_days`.
const FIVE_MONTHS: i64 = 5;

/// The rounding term of `civil_from_days`' month formula.
/// Format: `civil_from_days`.
const MONTH_ROUNDING: i64 = 2;

/// The months from March through December, after which `civil_from_days`' month index wraps to January.
/// Format: `civil_from_days`: `m = mp < 10 ? mp+3 : mp-9`.
const MARCH_TO_DECEMBER: i64 = 10;

/// The calendar month of `civil_from_days`' month index 0 (March), and its months to subtract past December.
/// Format: `civil_from_days`.
const MARCH_NUMBER: i64 = 3;

/// The months past December by which the month index wraps.
/// Format: `civil_from_days`: `mp - 9`.
const WRAP_MONTHS: i64 = 9;

/// The decimal base dates are written in.
/// Format: `svn__strtoul` and `TIMESTAMP_FORMAT` (`time.c`).
const DECIMAL: u64 = 10;

/// `SVN_ERR_RA_NOT_AUTHORIZED`, `must_have_access`'s refusal of a path the session may not read, made without a
/// message and so sent with the error's own, "Authorization failed".
/// Format: the RA category's start, 120,000 + 10 × 5,000 = 170,000, plus 1 (`svn_error_codes.h` 982-984).
const SVN_ERR_RA_NOT_AUTHORIZED: u32 = 170_001;

/// `SVN_ERR_AUTHZ_UNREADABLE`, the refusal of a path a command needs that the session may not read.
/// Format: the authz category's start, 120,000 + 20 × 5,000 = 220,000, plus 1 (`svn_error_codes.h` 1658-1660).
const SVN_ERR_AUTHZ_UNREADABLE: u32 = 220_001;

/// The revision properties a partly readable revision shows (`svn_repos_fs_revision_proplist`,
/// `svn_repos_fs_revision_prop`, `fill_log_entry`).
/// Format: `svn:author` and `svn:date`, as all three name them.
const PARTLY_READABLE_PROPERTIES: [&[u8]; 2] = [AUTHOR_PROPERTY, DATE_PROPERTY];

/// The session a command runs in, as the handshake (A-599) hands it over once the client is authenticated.
#[derive(Clone, Copy, Debug)]
pub struct Session<'session> {
  /// The repository's path the session's URL names, canonical (`svnserve`'s `fs_path`): every path a command names is
  /// beneath it, and the paths it answers with are relative to it.
  pub base: &'session str,
  /// The repository's UUID, which a node's properties carry as `svn:entry:uuid`.
  pub uuid: &'session str,
  /// What the client announced it takes (the handshake's, A-599): `get-file-revs` sends svndiff 2 to a client that
  /// accepts it, 0 to any other.
  pub capabilities: ClientCapabilities,
  /// The rules the repository's authorization file gives the session's principal (A-606: [`crate::authz::Authz`]'s
  /// rules for the repository's name and the session's user, compiled once); `None` when the repository names no
  /// authorization file, and then every path is readable (`authz_check_access` grants every path without one).
  pub authz: Option<&'session PrincipalRules>,
  /// Where the session's authentication stands for the command's path check (`must_have_access`).
  pub authentication: Authentication,
}

/// Where a session's authentication stands for the command a path check is made in (`serve.c` `must_have_access`,
/// 809-850).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authentication {
  /// No authentication can be asked for: a path the session may not read is refused "Authorization failed" (170001)
  /// with nothing before it, and a readable one is answered after the trivial authentication request.
  Settled,
  /// The session is anonymous and the client could authenticate for read access (`must_have_access`' condition: no
  /// user yet, authenticated users granted read access, and a way to authenticate): a path the session may not read is
  /// answered with nothing written, [`Outcome::Authenticate`], for the connection to ask the client.
  Available,
  /// The command runs again once the client has authenticated for it: its answer has no trivial authentication request
  /// before it, the authentication's own success standing for it, and a path the new principal may not read either is
  /// refused "Authorization failed" with nothing before it.
  Done,
}

/// How much of a revision the session may read (`svn_repos_revision_access_level_t`), judged from its changed paths
/// and the sources of those copied (`svn_repos_check_revision_access`, `detect_changed`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RevisionAccess {
  /// Every changed path and copy source is readable, or the revision changed nothing: every revision property shows.
  Full,
  /// Some changed path or copy source is not: the author and the date alone show.
  Partial,
  /// No changed path is readable: no revision property shows.
  Nothing,
}

impl RevisionAccess {
  /// The level decided from what was found (`decision:`): nothing readable, some of each, or all readable.
  fn of(found_readable: bool, found_unreadable: bool) -> Self {
    match (found_readable, found_unreadable) {
      (false, _) => Self::Nothing,
      (true, true) => Self::Partial,
      (true, false) => Self::Full,
    }
  }

  /// Whether a revision property named `name` shows at this level.
  fn shows(self, name: &[u8]) -> bool {
    match self {
      Self::Full => true,
      Self::Partial => PARTLY_READABLE_PROPERTIES.contains(&name),
      Self::Nothing => false,
    }
  }
}

/// What the read commands read from the green beyond the index and the mergeinfo queries' [`Contents`]: revision
/// properties (§4.17.4 item 6) and property lists (item 1).
pub trait Repository: Contents {
  /// Revision `revision`'s properties, every one, in any order; `None` for a revision the green does not hold.
  fn revision_properties(&self, revision: Revision) -> Option<Vec<(&[u8], &[u8])>>;

  /// The property list held as `identity`, every property, in any order; `None` when the green does not hold it.
  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>>;

  /// The bytes of the text held as `identity` from `offset`: a run of at least one byte, as long as the green holds it
  /// in one piece (§4.17.4 item 11: a read streams from chunks, nothing is assembled whole), or none at the text's end;
  /// `None` when the green does not hold the text.
  fn text(&self, identity: &ContentIdentity, offset: u64) -> Option<&[u8]>;
}

/// What the handler made of a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
  /// Answered: its answer, or its refusal, is appended to the output.
  Answered,
  /// Answered in part: the answer's head is appended to the output, and its rest follows a slice at a time from the
  /// [`Stream`] ([`Stream::resume`]).
  Streaming(Stream),
  /// Malformed: not a command, or parameters `svn_ra_svn__parse_tuple` refuses (`SVN_ERR_RA_SVN_MALFORMED_DATA`), which
  /// `svnserve` answers by ending the connection without a word. Nothing is appended; the caller ends the connection.
  Close,
  /// A command of the main set another piece answers. Nothing is appended.
  Elsewhere,
  /// The path the command names is one the anonymous session may not read, and the client could authenticate
  /// ([`Authentication::Available`]): nothing is appended. The caller asks the client to authenticate, as
  /// `must_have_access` does with a username needed, then runs the command again with [`Authentication::Done`].
  Authenticate,
}

/// Answers one command of a session: appends its answer to `out` and says what it made of it.
///
/// # Errors
/// [`ItemError`] when an answer cannot be written, which no answer here can give: every word written is the
/// protocol's.
pub fn answer<R: Repository + ?Sized>(
  index: &Index,
  repository: &R,
  session: &Session<'_>,
  command: &Value<'_>,
  out: &mut Vec<u8>,
) -> Result<Outcome, ItemError> {
  let Value::List(parts) = command else {
    return Ok(Outcome::Close);
  };
  let (Some(Value::Word(name)), Some(Value::List(params))) = (parts.first(), parts.get(1)) else {
    return Ok(Outcome::Close);
  };
  let mut reads = Reads {
    index,
    repository,
    base: session.base,
    uuid: session.uuid,
    authz: session.authz,
    authentication: session.authentication,
    out,
  };
  match *name {
    "get-latest-rev" => reads.latest(),
    "get-dated-rev" => reads.dated(params),
    "rev-proplist" => reads.revision_list(params),
    "rev-prop" => reads.revision_property_command(params),
    "check-path" => reads.check_path(params),
    "stat" => reads.stat(params),
    "get-mergeinfo" => reads.mergeinfo(params),
    "get-iprops" => reads.inherited(params),
    "get-dir" => reads.directory(params),
    "get-locations" => reads.locations(params),
    "get-file" => reads.file(params),
    "list" => reads.list(params),
    "get-location-segments" => reads.location_segments(params),
    "get-deleted-rev" => reads.deleted_revision(params),
    "log" => reads.log(params),
    "get-file-revs" => reads.file_revisions(params, session.capabilities),
    other if ELSEWHERE.contains(&other) => Ok(Outcome::Elsewhere),
    other => reads.unknown(other),
  }
}

/// `get-lock` and `get-locks` ([`locks`]' documentation), answered from the repository's lock table `locks` at the
/// owner's instant `now`; `None` for any other command, which [`answer`] answers.
///
/// # Errors
/// [`ItemError`] when an answer cannot be written, which no answer here can give.
pub fn answer_lock_read<R: Repository + ?Sized>(
  index: &Index,
  repository: &R,
  (locks, now): (&LockTable, AprTime),
  session: &Session<'_>,
  command: &Value<'_>,
  out: &mut Vec<u8>,
) -> Option<Result<Outcome, ItemError>> {
  let Value::List(parts) = command else {
    return None;
  };
  let (Some(Value::Word(name)), Some(Value::List(params))) = (parts.first(), parts.get(1)) else {
    return None;
  };
  if *name != "get-lock" && *name != "get-locks" {
    return None;
  }
  let mut reads = Reads {
    index,
    repository,
    base: session.base,
    uuid: session.uuid,
    authz: session.authz,
    authentication: session.authentication,
    out,
  };
  Some(if *name == "get-lock" {
    reads.get_lock(params, locks, now)
  } else {
    reads.get_locks(params, locks, now)
  })
}

/// `svn_repos_check_revision_access` (`repos.c`): how much of `revision` a session reading by `authz` may read. Without
/// rules, or for a revision that changed nothing, all of it; else from each changed path and, for one a copy added, its
/// source, until one of each kind is found.
fn revision_access_in(
  index: &Index,
  authz: Option<&PrincipalRules>,
  revision: Revision,
) -> Result<RevisionAccess, Failure> {
  if authz.is_none() {
    return Ok(RevisionAccess::Full);
  }
  let changes = index
    .changes(revision)
    .map_err(|refusal| Failure::of_index(&refusal))?;
  if changes.is_empty() {
    return Ok(RevisionAccess::Full);
  }
  let (mut found_readable, mut found_unreadable) = (false, false);
  for change in changes {
    if may_read(authz, &change.path) {
      found_readable = true;
    } else {
      found_unreadable = true;
    }
    if change
      .copy_from
      .as_ref()
      .is_some_and(|source| !may_read(authz, &source.path))
    {
      found_unreadable = true;
    }
    if found_readable && found_unreadable {
      break;
    }
  }
  Ok(RevisionAccess::of(found_readable, found_unreadable))
}

/// Whether a session reading by `authz` may read all of `revision` (`svn_repos_check_revision_access`'s
/// `svn_repos_revision_access_full`), what a revision property's change asks first (A-702); refused with the error
/// number and words of a revision the index does not hold.
pub(crate) fn revision_wholly_readable(
  index: &Index,
  authz: Option<&PrincipalRules>,
  revision: Revision,
) -> Result<bool, (u32, String)> {
  revision_access_in(index, authz, revision)
    .map(|access| matches!(access, RevisionAccess::Full))
    .map_err(|failure| (failure.code, failure.message))
}

/// A refusal as `svnserve` writes it: Subversion's error number and its message.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Failure {
  /// The error number.
  code: u32,
  /// The message, as the C code formats it.
  message: String,
}

impl Failure {
  /// A refusal.
  fn new(code: u32, message: String) -> Self {
    Self { code, message }
  }

  /// FSFS's refusal of a revision: "Invalid revision number" below 0, "No such revision" past the youngest
  /// (`svn_fs_fs__ensure_revision_exists`).
  fn revision(revision: i64) -> Self {
    let message = if revision < 0 {
      format!("Invalid revision number '{revision}'")
    } else {
      format!("No such revision {revision}")
    };
    Self::new(SVN_ERR_FS_NO_SUCH_REVISION, message)
  }

  /// FSFS's refusal of a path absent in a revision (`SVN_FS__NOT_FOUND`).
  fn not_found(path: &str, revision: Revision) -> Self {
    Self::new(
      SVN_ERR_FS_NOT_FOUND,
      format!("File not found: revision {revision}, path '{path}'"),
    )
  }

  /// "Bogus date": `SVN_ERR_BAD_DATE`'s own message, which `svnserve` writes for an error made without one.
  fn bogus_date() -> Self {
    Self::new(SVN_ERR_BAD_DATE, String::from("Bogus date"))
  }

  /// A filesystem the index and the green disagree about.
  fn corrupt() -> Self {
    Self::new(SVN_ERR_FS_CORRUPT, String::from("Filesystem is corrupt"))
  }

  /// `must_have_access`' refusal, made without a message: "Authorization failed", the error's own.
  fn not_authorized() -> Self {
    Self::new(
      SVN_ERR_RA_NOT_AUTHORIZED,
      String::from("Authorization failed"),
    )
  }

  /// `SVN_ERR_AUTHZ_UNREADABLE` made without a message (`get_path_histories`, `svn_repos_get_logs5`'s shortcut):
  /// "Item is not readable", the error's own.
  fn unreadable_item() -> Self {
    Self::new(
      SVN_ERR_AUTHZ_UNREADABLE,
      String::from("Item is not readable"),
    )
  }

  /// `check_readability`'s refusal (`rev_hunt.c`): "Unreadable path encountered; access denied".
  fn unreadable_path() -> Self {
    Self::new(
      SVN_ERR_AUTHZ_UNREADABLE,
      String::from("Unreadable path encountered; access denied"),
    )
  }

  /// `( failure ( ( code message 0: 0 ) ) )`, a release build's empty file and zero line.
  fn value(&self) -> Value<'_> {
    let error = Value::List(vec![
      Value::Number(u64::from(self.code)),
      Value::String(self.message.as_bytes()),
      Value::String(b""),
      Value::Number(0),
    ]);
    Value::List(vec![Value::Word(FAILURE), Value::List(vec![error])])
  }

  /// The refusal of an index refusal the read commands meet.
  fn of_index(refusal: &IndexRefusal) -> Self {
    match refusal {
      IndexRefusal::NoSuchRevision { revision } => {
        Self::revision(i64::try_from(*revision).unwrap_or(i64::MAX))
      }
      IndexRefusal::NotFound { path, revision } => Self::not_found(path, *revision),
      _ => Self::corrupt(),
    }
  }

  /// The refusal of a mergeinfo query's refusal: a requested path absent, or one the index cannot hold (a `..` name),
  /// is not found, as FSFS's `open_path` refuses it.
  fn of_query(refusal: &QueryRefusal, revision: Revision) -> Self {
    match refusal {
      QueryRefusal::Index(IndexRefusal::PathSyntax { path }) => Self::not_found(path, revision),
      QueryRefusal::Index(refusal) => Self::of_index(refusal),
      QueryRefusal::ClaimsMergeinfo { path, revision } => Self::new(
        SVN_ERR_FS_CORRUPT,
        format!("Node-revision '{path}@{revision}' claims to have mergeinfo but doesn't"),
      ),
      QueryRefusal::ContentsDisagree { .. } => Self::corrupt(),
    }
  }
}

/// One command's work: the index, the green, the session's base and the output.
struct Reads<'reads, R: ?Sized> {
  /// The index.
  index: &'reads Index,
  /// The green.
  repository: &'reads R,
  /// The session's base path.
  base: &'reads str,
  /// The repository's UUID.
  uuid: &'reads str,
  /// The session's read rules, `None` reading every path.
  authz: Option<&'reads PrincipalRules>,
  /// Where the session's authentication stands for the command's path check.
  authentication: Authentication,
  /// The output.
  out: &'reads mut Vec<u8>,
}

impl<'reads, R: Repository + ?Sized> Reads<'reads, R> {
  /// `get-latest-rev`: the youngest revision.
  fn latest(&mut self) -> Result<Outcome, ItemError> {
    self.trivial_auth()?;
    let youngest = self.index.youngest();
    self.success(vec![Value::Number(youngest)])
  }

  /// `get-dated-rev ( date:string )`: the youngest revision dated at or before the date (`svn_repos_dated_revision`).
  fn dated(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some(date) = c_string(params.first()) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
    let found = parse_time(date)
      .ok_or_else(Failure::bogus_date)
      .and_then(|time| self.dated_revision(time));
    match found {
      Ok(revision) => self.success(vec![Value::Number(revision)]),
      Err(failure) => self.failure(&failure),
    }
  }

  /// `svn_repos_dated_revision`'s binary search over the revisions' dates, its edges as Subversion takes them.
  fn dated_revision(&self, time: i64) -> Result<Revision, Failure> {
    let latest = self.index.youngest();
    let (mut bottom, mut top) = (0_u64, latest);
    while bottom <= top {
      let middle = bottom.saturating_add(top) / 2;
      let this = self.time_of(middle)?;
      let step = if this > time {
        self.overshot(middle, time)?
      } else if this < time {
        self.undershot(middle, latest, time)?
      } else {
        Search::Found(middle)
      };
      match step {
        Search::Found(revision) => return Ok(revision),
        Search::Below(next) => top = next,
        Search::Above(next) => bottom = next,
      }
    }
    Ok(0)
  }

  /// The search's step past the date at `middle`: revision 0 at the bottom, the one before when its date is not past
  /// the date, else on below it.
  fn overshot(&self, middle: Revision, time: i64) -> Result<Search, Failure> {
    let Some(previous) = middle.checked_sub(1) else {
      return Ok(Search::Found(0));
    };
    Ok(if self.time_of(previous)? <= time {
      Search::Found(previous)
    } else {
      Search::Below(previous)
    })
  }

  /// The search's step short of the date at `middle`: the youngest at the top, `middle` when the next revision's date
  /// is past the date, else on above it.
  fn undershot(&self, middle: Revision, latest: Revision, time: i64) -> Result<Search, Failure> {
    let next = middle.saturating_add(1);
    if next > latest {
      return Ok(Search::Found(latest));
    }
    Ok(if self.time_of(next)? > time {
      Search::Found(middle)
    } else {
      Search::Above(next)
    })
  }

  /// `rev_hunt.c` `get_time`: a revision's date, refused when it has none or one that does not parse.
  fn time_of(&self, revision: Revision) -> Result<i64, Failure> {
    let date = self
      .revision_property(revision, DATE_PROPERTY)
      .ok_or_else(|| {
        Failure::new(
          SVN_ERR_FS_GENERAL,
          format!("Failed to find time on revision {revision}"),
        )
      })?;
    parse_time(c_string_of(date)).ok_or_else(Failure::bogus_date)
  }

  /// `rev-proplist ( rev:number )`: a revision's properties, as many as the session may read of the revision
  /// (`svn_repos_fs_revision_proplist`: all, the author and the date, or none).
  fn revision_list(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some(revision) = revnum(params.first()) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
    let found = self
      .existing(revision)
      .and_then(|revision| Ok((revision, self.revision_access(revision)?)));
    match found {
      Ok((revision, access)) => {
        let properties = self
          .repository
          .revision_properties(revision)
          .unwrap_or_default()
          .into_iter()
          .filter(|(name, _)| access.shows(name))
          .collect();
        self.success(vec![proplist(properties)])
      }
      Err(failure) => self.failure(&failure),
    }
  }

  /// `rev-prop ( rev:number name:string )`: one revision property, or none, none too where the session may not read it
  /// (`svn_repos_fs_revision_prop`).
  fn revision_property_command(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let (Some(revision), Some(name)) = (revnum(params.first()), c_string(params.get(1))) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
    let found = self
      .existing(revision)
      .and_then(|revision| Ok((revision, self.revision_access(revision)?)));
    match found {
      Ok((revision, access)) => {
        let value = self
          .revision_property(revision, name)
          .filter(|_| access.shows(name));
        self.success(vec![Value::List(
          value.map(Value::String).into_iter().collect(),
        )])
      }
      Err(failure) => self.failure(&failure),
    }
  }

  /// `check-path ( path:string [ rev:number ] )`: the kind of node at the path, `none` where there is none; refused
  /// first where the session may not read the path (`must_have_access`).
  fn check_path(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some((path, revision)) = path_and_revision(params, self.base) else {
      return Ok(Outcome::Close);
    };
    if let Some(outcome) = self.must_read(&path)? {
      return Ok(outcome);
    }
    let found = self
      .at_or_youngest(revision)
      .and_then(|revision| self.node(&path, revision));
    match found {
      Ok(node) => {
        let kind = node.map_or(NONE_WORD, |node| kind_word(node.kind()));
        self.success(vec![Value::Word(kind)])
      }
      Err(failure) => self.failure(&failure),
    }
  }

  /// `stat ( path:string [ rev:number ] )`: the node's dirent (`svn_repos_stat`), or nothing where there is none;
  /// refused first where the session may not read the path (`must_have_access`).
  fn stat(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some((path, revision)) = path_and_revision(params, self.base) else {
      return Ok(Outcome::Close);
    };
    if let Some(outcome) = self.must_read(&path)? {
      return Ok(outcome);
    }
    let found = self
      .at_or_youngest(revision)
      .and_then(|revision| self.node(&path, revision))
      .and_then(|node| node.map(|node| self.dirent(&node)).transpose());
    match found {
      Ok(Some(dirent)) => {
        let item = dirent.value();
        self.success(vec![Value::List(vec![item])])
      }
      Ok(None) => self.success(vec![Value::List(Vec::new())]),
      Err(failure) => self.failure(&failure),
    }
  }

  /// A node's dirent as `stat` writes it (`svn_repos_stat`): kind, size, whether it has properties, created revision,
  /// and that revision's date and author when it has them; a date that does not parse is refused.
  fn dirent(&self, node: &Node<'reads>) -> Result<Dirent<'reads>, Failure> {
    let size = match node.kind() {
      NodeKind::Directory => DIRECTORY_SIZE,
      NodeKind::File => node.text().map_or(0, |text| text.size),
    };
    let created = node.created_revision();
    let date = match self.revision_property(created, DATE_PROPERTY) {
      Some(date) => Some(format_time(
        parse_time(c_string_of(date)).ok_or_else(Failure::bogus_date)?,
      )),
      None => None,
    };
    Ok(Dirent {
      kind: node.kind(),
      size,
      has_properties: self.has_properties(node),
      created,
      date,
      author: self.revision_property(created, AUTHOR_PROPERTY),
    })
  }

  /// `get-mergeinfo ( ( path:string ... ) [ rev:number ] inherit:word descendants:bool )`: the mergeinfo of each path,
  /// keyed by its path relative to the session.
  fn mergeinfo(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some(request) = MergeinfoRequest::parse(params, self.base) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
    let found = self.mergeinfo_catalog(&request);
    match found {
      Ok(catalog) => {
        let entries: Vec<(String, Vec<u8>)> = catalog
          .into_iter()
          .map(|(path, mergeinfo)| (relative_to(self.base, &path), mergeinfo))
          .collect();
        let items = entries
          .iter()
          .map(|(path, value)| {
            Value::List(vec![Value::String(path.as_bytes()), Value::String(value)])
          })
          .collect();
        self.success(vec![Value::List(items)])
      }
      Err(failure) => self.failure(&failure),
    }
  }

  /// `svn_repos_fs_get_mergeinfo2`'s catalog for `request`: each readable path's mergeinfo (and its descendants' where
  /// asked), each as its repository path and its mergeinfo's string, at the revision asked or the youngest.
  fn mergeinfo_catalog(
    &self,
    request: &MergeinfoRequest,
  ) -> Result<Vec<(String, Vec<u8>)>, Failure> {
    self.at_or_youngest(request.revision).and_then(|revision| {
      let paths: Vec<&str> = request
        .paths
        .iter()
        .map(String::as_str)
        .filter(|path| self.readable(path))
        .collect();
      if paths.is_empty() {
        return Ok(Vec::new());
      }
      mergeinfo::catalog(
        self.index,
        self.repository,
        &paths,
        revision,
        request.inheritance,
        request.descendants,
      )
      .map(|catalog| {
        catalog
          .into_iter()
          .map(|(path, mergeinfo)| (path, mergeinfo.to_bytes()))
          .collect()
      })
      .map_err(|refusal| Failure::of_query(&refusal, revision))
    })
  }

  /// `get-iprops ( path:string [ rev:number ] )`: the property lists of the path's ancestors that have any and the
  /// session may read, the root's first (`svn_repos_fs_get_inherited_props`); refused first where the session may not
  /// read the path (`must_have_access`). An absent revision is not the youngest here: `svnserve` opens the root of the
  /// invalid revision, and so refuses it.
  fn inherited(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some((path, revision)) = path_and_revision(params, self.base) else {
      return Ok(Outcome::Close);
    };
    if let Some(outcome) = self.must_read(&path)? {
      return Ok(outcome);
    }
    let found = self
      .existing(revision.unwrap_or(-1))
      .and_then(|revision| self.present_for_iprops(&path, revision))
      .and_then(|revision| self.ancestors_properties(&path, revision));
    match found {
      Ok(ancestors) => {
        let items = ancestors
          .into_iter()
          .map(|(ancestor, properties)| {
            Value::List(vec![
              Value::String(ancestor.as_bytes()),
              proplist(properties),
            ])
          })
          .collect();
        self.success(vec![Value::List(items)])
      }
      Err(failure) => self.failure(&failure),
    }
  }

  /// `get_inherited_props`' check before the walk: the path must be present, else "'P' path not found".
  fn present_for_iprops(&self, path: &str, revision: Revision) -> Result<Revision, Failure> {
    match self.node(path, revision)? {
      Some(_) => Ok(revision),
      None => Err(Failure::new(
        SVN_ERR_FS_NOT_FOUND,
        format!("'{path}' path not found"),
      )),
    }
  }

  /// The ancestors of `path` holding properties that the session may read, root first, each by its path without the
  /// leading separator (`svn_repos_fs_get_inherited_props`: an unreadable ancestor is passed over).
  fn ancestors_properties<'path>(
    &self,
    path: &'path str,
    revision: Revision,
  ) -> Result<Ancestors<'path, 'reads>, Failure> {
    let mut found = Vec::new();
    let mut place = parent(path);
    while let Some(ancestor) = place {
      if self.readable(ancestor)
        && let Some(node) = self.node(ancestor, revision)?
        && let PropertyList::Held { identity, .. } = node.properties()
      {
        let properties = self.repository.properties(&identity).unwrap_or_default();
        if !properties.is_empty() {
          found.push((
            ancestor.strip_prefix(SEPARATOR).unwrap_or(ancestor),
            properties,
          ));
        }
      }
      place = parent(ancestor);
    }
    found.reverse();
    Ok(found)
  }

  /// The ancestors of `path` holding properties, root first, each by its own path without the leading separator.
  fn owned_ancestors(
    &self,
    path: &str,
    revision: Revision,
  ) -> Result<OwnedAncestors<'reads>, Failure> {
    let found = self.ancestors_properties(path, revision)?;
    Ok(
      found
        .into_iter()
        .map(|(ancestor, properties)| (ancestor.to_owned(), properties))
        .collect(),
    )
  }

  /// `get-dir ( path [ rev ] want-props want-contents ? ( field ... ) ? want-iprops )`: the directory's properties
  /// with its entry properties, its entries as dirents carrying the fields asked for, and its ancestors' properties
  /// when asked (`serve.c` `get_dir`).
  fn directory(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some(request) = DirectoryRequest::parse(params, self.base) else {
      return Ok(Outcome::Close);
    };
    if let Some(outcome) = self.must_read(&request.path)? {
      return Ok(outcome);
    }
    match self.directory_answer(&request) {
      Ok(answer) => {
        let mut items = vec![
          Value::Number(answer.revision),
          owned_proplist(&answer.properties),
        ];
        items.push(Value::List(
          answer.entries.iter().map(DirectoryEntry::value).collect(),
        ));
        if let Some(ancestors) = &answer.ancestors {
          items.push(inherited_value(ancestors));
        }
        self.success(items)
      }
      Err(failure) => self.failure(&failure),
    }
  }

  /// `get_dir`'s work in its order: the revision, the properties, then the entries.
  fn directory_answer(
    &self,
    request: &DirectoryRequest,
  ) -> Result<DirectoryAnswer<'reads>, Failure> {
    let revision = self.at_or_youngest(request.revision)?;
    let properties = if request.properties {
      self.node_properties(&request.path, revision)?
    } else {
      Vec::new()
    };
    let ancestors = if request.inherited {
      Some(self.owned_ancestors(&request.path, revision)?)
    } else {
      None
    };
    let entries = if request.contents {
      self.entries(&request.path, revision, request.fields)?
    } else {
      Vec::new()
    };
    Ok(DirectoryAnswer {
      revision,
      properties,
      entries,
      ancestors,
    })
  }

  /// A node's properties and the entry properties `get_props` adds: its committed revision, that revision's date and
  /// author as stored, and the repository's UUID.
  fn node_properties(&self, path: &str, revision: Revision) -> Result<PropertyPairs, Failure> {
    let node = self
      .node(path, revision)?
      .ok_or_else(|| Failure::not_found(path, revision))?;
    let mut properties: Vec<(Vec<u8>, Vec<u8>)> = match node.properties() {
      PropertyList::Empty => Vec::new(),
      PropertyList::Held { identity, .. } => self
        .repository
        .properties(&identity)
        .unwrap_or_default()
        .into_iter()
        .map(|(name, value)| (name.to_vec(), value.to_vec()))
        .collect(),
    };
    let created = node.created_revision();
    properties.push((
      ENTRY_COMMITTED_REV.to_vec(),
      created.to_string().into_bytes(),
    ));
    if let Some(date) = self.revision_property(created, DATE_PROPERTY) {
      properties.push((ENTRY_COMMITTED_DATE.to_vec(), date.to_vec()));
    }
    if let Some(author) = self.revision_property(created, AUTHOR_PROPERTY) {
      properties.push((ENTRY_LAST_AUTHOR.to_vec(), author.to_vec()));
    }
    properties.push((ENTRY_UUID.to_vec(), self.uuid.as_bytes().to_vec()));
    Ok(properties)
  }

  /// A directory's entries as `get_dir` writes them (`svn_fs_dir_entries`), each one the session may not read left out
  /// (`lookup_access`); refused for an absent path and for a file.
  fn entries(
    &self,
    path: &str,
    revision: Revision,
    fields: Fields,
  ) -> Result<Vec<DirectoryEntry<'reads>>, Failure> {
    let node = self
      .node(path, revision)?
      .ok_or_else(|| Failure::not_found(path, revision))?;
    if node.kind() != NodeKind::Directory {
      return Err(Failure::new(
        SVN_ERR_FS_NOT_DIRECTORY,
        String::from("Can't get entries of non-directory"),
      ));
    }
    let names = self
      .repository
      .entries(path, revision)
      .ok_or_else(Failure::corrupt)?;
    names
      .into_iter()
      .filter(|name| self.readable(&child_path(path, name)))
      .map(|name| self.dirent_of(path, name, revision, fields))
      .collect()
  }

  /// One entry's dirent: each field the request asked for, and `get_dir`'s defaults for the rest (kind `none`, size 0,
  /// no properties, revision 0, the date of time 0).
  fn dirent_of(
    &self,
    directory: &str,
    name: String,
    revision: Revision,
    fields: Fields,
  ) -> Result<DirectoryEntry<'reads>, Failure> {
    let path = child_path(directory, &name);
    let node = self.node(&path, revision)?.ok_or_else(Failure::corrupt)?;
    let size = match node.kind() {
      NodeKind::File if fields.size => node.text().map_or(0, |text| text.size),
      _ => 0,
    };
    let committed = fields.reads_commit().then(|| node.created_revision());
    Ok(DirectoryEntry {
      name,
      kind: fields.kind.then(|| node.kind()),
      size,
      has_properties: fields.has_properties && self.has_properties(&node),
      created: committed.unwrap_or(0),
      date: committed.and_then(|created| self.revision_property(created, DATE_PROPERTY)),
      author: committed.and_then(|created| self.revision_property(created, AUTHOR_PROPERTY)),
    })
  }

  /// `get-locations ( path peg-rev ( rev ... ) )`: where the node at the path in the peg revision stood in each
  /// revision (`svn_repos_trace_node_locations`), each as `( rev path )`, then `done`, then the answer or the refusal.
  fn locations(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some((path, peg, revisions)) = locations_request(params, self.base) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
    let found = self.trace_locations(&path, peg, revisions);
    if let Ok(locations) = &found {
      for (revision, place) in locations {
        let entry = Value::List(vec![
          Value::Number(*revision),
          Value::String(place.as_bytes()),
        ]);
        write(&entry, self.out)?;
      }
    }
    write(&Value::Word(DONE), self.out)?;
    match found {
      Ok(_) => self.success(Vec::new()),
      Err(failure) => self.failure(&failure),
    }
  }

  /// `svn_repos_trace_node_locations`: the revisions younger than the peg kept while the node's line reaches back to
  /// the same path, then each copy crossed back to its source, then the revisions where the path holds the same line.
  /// With rules, the peg revision is opened first and an unreadable path refused (`check_readability`), and a copy's
  /// source the session may not read ends the trace with the locations found before it.
  fn trace_locations(
    &self,
    path: &str,
    peg: i64,
    mut revisions: Vec<i64>,
  ) -> Result<Vec<(u64, String)>, Failure> {
    if self.authz.is_some() {
      self.existing(peg)?;
      if !self.readable(path) {
        return Err(Failure::unreadable_path());
      }
    }
    revisions.sort_unstable_by(|left, right| right.cmp(left));
    let mut wanted = revisions.into_iter().peekable();
    let mut start = peg;
    while let Some(&future) = wanted.peek().filter(|&&future| future > peg) {
      if self.reaches_back(path, peg, future)? {
        start = future;
        break;
      }
      wanted.next();
    }
    let (mut place, mut revision) = (path.to_owned(), start);
    let mut found = Vec::new();
    while wanted.peek().is_some() {
      let Some((appeared, previous, previous_revision)) =
        self.previous_location(&place, revision)?
      else {
        break;
      };
      while let Some(&at) = wanted.peek().filter(|&&at| at >= appeared) {
        found.push((
          Revision::try_from(at).map_err(|_| Failure::revision(at))?,
          place.clone(),
        ));
        wanted.next();
      }
      while wanted.next_if(|&at| at > previous_revision).is_some() {}
      (place, revision) = (previous, previous_revision);
      if !self.readable(&place) {
        return Ok(found);
      }
    }
    let line = self
      .existing(revision)
      .and_then(|revision| self.node(&place, revision))?;
    for at in wanted {
      let held = self.existing(at).and_then(|at| self.node(&place, at))?;
      match (held, line) {
        (Some(node), Some(line)) if node.node_id() == line.node_id() => {
          found.push((node_revision_of(at)?, place.clone()));
        }
        _ => break,
      }
    }
    Ok(found)
  }

  /// `check_ancestry_of_peg_path`: whether the node at `path` in `future` stood at the same path in `peg`; refused
  /// when the path is absent in `future`, as its history cannot be opened.
  fn reaches_back(&self, path: &str, peg: i64, future: i64) -> Result<bool, Failure> {
    let future_revision = self.existing(future)?;
    let node = self
      .node(path, future_revision)?
      .ok_or_else(|| Failure::not_found(path, future_revision))?;
    let (mut place, mut revision) = (path.to_owned(), future);
    while let Some((appeared, previous, previous_revision)) =
      self.previous_location(&place, revision)?
    {
      if appeared <= peg {
        break;
      }
      (place, revision) = (previous, previous_revision);
    }
    let at_peg = self.existing(peg).and_then(|peg| self.node(&place, peg))?;
    Ok(place == path && at_peg.is_some_and(|held| held.node_id() == node.node_id()))
  }

  /// `svn_repos__prev_location`: the innermost copy relevant to `path` in `revision` (`svn_fs_closest_copy`), as the
  /// revision it appeared in and the path and revision it was copied from, extended to `path`; `None` without one.
  fn previous_location(
    &self,
    path: &str,
    revision: i64,
  ) -> Result<Option<(i64, String, i64)>, Failure> {
    let revision = self.existing(revision)?;
    let Some((copied_in, copy_path)) = self.closest_copy(path, revision)? else {
      return Ok(None);
    };
    let Some((source, source_revision)) = self
      .node(&copy_path, copied_in)?
      .and_then(|root| root.copy_from())
    else {
      return Ok(None);
    };
    let remainder = path.strip_prefix(copy_path.as_str()).unwrap_or_default();
    let previous = if source == ROOT {
      remainder.to_owned()
    } else {
      format!("{source}{remainder}")
    };
    let previous = if previous.is_empty() {
      ROOT.to_owned()
    } else {
      previous
    };
    Ok(Some((
      i64::try_from(copied_in).unwrap_or(i64::MAX),
      previous,
      i64::try_from(source_revision).unwrap_or(i64::MAX),
    )))
  }

  /// `svn_fs_closest_copy` (`tree.c` `fs_closest_copy`), asked of the crate's one walk of copy roots
  /// (`crate::report::drive::history`): the revision and path of the copy that made the node at `path` in `revision`
  /// what it is; `None` without one. Refused when the path is absent, as FSFS's `open_path` refuses it.
  fn closest_copy(
    &self,
    path: &str,
    revision: Revision,
  ) -> Result<Option<(Revision, String)>, Failure> {
    if self.node(path, revision)?.is_none() {
      return Err(Failure::not_found(path, revision));
    }
    copies::closest_copy(self.index, path, revision).map_err(|refusal| Failure::of_index(&refusal))
  }

  /// An unknown command: "Unknown editor command", with no authentication request first.
  fn unknown(&mut self, name: &str) -> Result<Outcome, ItemError> {
    self.failure(&Failure::new(
      SVN_ERR_RA_SVN_UNKNOWN_CMD,
      format!("Unknown editor command '{name}'"),
    ))
  }

  /// The node at `path` in `revision`, `None` where there is none; a path the index cannot hold names nothing.
  fn node(&self, path: &str, revision: Revision) -> Result<Option<Node<'reads>>, Failure> {
    match self.index.node(path, revision) {
      Ok(node) => Ok(node),
      Err(IndexRefusal::PathSyntax { .. }) => Ok(None),
      Err(refusal) => Err(Failure::of_index(&refusal)),
    }
  }

  /// Whether a node's property list holds any property.
  fn has_properties(&self, node: &Node<'_>) -> bool {
    match node.properties() {
      PropertyList::Empty => false,
      PropertyList::Held { identity, .. } => self
        .repository
        .properties(&identity)
        .is_some_and(|properties| !properties.is_empty()),
    }
  }

  /// One revision property's value.
  fn revision_property(&self, revision: Revision, name: &[u8]) -> Option<&'reads [u8]> {
    self
      .repository
      .revision_properties(revision)?
      .into_iter()
      .find(|(property, _)| *property == name)
      .map(|(_, value)| value)
  }

  /// A revision a command names, refused as FSFS refuses it.
  fn existing(&self, revision: i64) -> Result<Revision, Failure> {
    Revision::try_from(revision)
      .ok()
      .filter(|&revision| revision <= self.index.youngest())
      .ok_or_else(|| Failure::revision(revision))
  }

  /// A command's optional revision: the youngest when absent, else as [`Reads::existing`] takes it.
  fn at_or_youngest(&self, revision: Option<i64>) -> Result<Revision, Failure> {
    match revision {
      None => Ok(self.index.youngest()),
      Some(revision) => self.existing(revision),
    }
  }

  /// Whether the session may read `path`: the session's one read rule ([`crate::authz::may_read`]), which an update's
  /// target and its drive ask too.
  fn readable(&self, path: &str) -> bool {
    may_read(self.authz, path)
  }

  /// `must_have_access` for reading `path` (`serve.c` 809-850): `None` when the session may read it, the command going
  /// on after the trivial authentication request (none once the client has just authenticated for it); else the
  /// command's outcome: [`Outcome::Authenticate`], nothing written, where the client could authenticate, or the
  /// refusal "Authorization failed" with nothing before it.
  fn must_read(&mut self, path: &str) -> Result<Option<Outcome>, ItemError> {
    if self.readable(path) {
      if self.authentication != Authentication::Done {
        self.trivial_auth()?;
      }
      return Ok(None);
    }
    if self.authentication == Authentication::Available {
      return Ok(Some(Outcome::Authenticate));
    }
    self.failure(&Failure::not_authorized())?;
    Ok(Some(Outcome::Answered))
  }

  /// `svn_repos_check_revision_access`: how much of `revision` the session may read ([`revision_access_in`]).
  fn revision_access(&self, revision: Revision) -> Result<RevisionAccess, Failure> {
    revision_access_in(self.index, self.authz, revision)
  }

  /// `trivial_auth_request`: `( success ( ( ) 0: ) )`.
  fn trivial_auth(&mut self) -> Result<(), ItemError> {
    let request = Value::List(vec![
      Value::Word(SUCCESS),
      Value::List(vec![Value::List(Vec::new()), Value::String(b"")]),
    ]);
    write(&request, self.out)
  }

  /// `( success ( params ) )`.
  fn success(&mut self, params: Vec<Value<'_>>) -> Result<Outcome, ItemError> {
    write(
      &Value::List(vec![Value::Word(SUCCESS), Value::List(params)]),
      self.out,
    )?;
    Ok(Outcome::Answered)
  }

  /// `( failure ( ( code message 0: 0 ) ) )`, a release build's empty file and zero line.
  fn failure(&mut self, failure: &Failure) -> Result<Outcome, ItemError> {
    write(&failure.value(), self.out)?;
    Ok(Outcome::Answered)
  }
}

/// Where one step of the dated search goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Search {
  /// The revision is found.
  Found(Revision),
  /// It lies at or below this revision.
  Below(Revision),
  /// It lies at or above this revision.
  Above(Revision),
}

/// The ancestors of a path holding properties, each by its path without the leading separator, with its properties.
type Ancestors<'path, 'reads> = Vec<(&'path str, Vec<(&'reads [u8], &'reads [u8])>)>;

/// [`Ancestors`] with each path owned.
type OwnedAncestors<'reads> = Vec<(String, Vec<(&'reads [u8], &'reads [u8])>)>;

/// An inherited property list as `get-dir`, `get-file` and `get-iprops` write it: `( ( path ( props ) ) ... )`, the
/// root's first.
fn inherited_value<'value>(ancestors: &'value OwnedAncestors<'_>) -> Value<'value> {
  Value::List(
    ancestors
      .iter()
      .map(|(path, properties)| {
        Value::List(vec![
          Value::String(path.as_bytes()),
          proplist(properties.clone()),
        ])
      })
      .collect(),
  )
}

/// The dirent fields a `get-dir` asks for (`SVN_DIRENT_*`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Fields {
  /// `kind`.
  kind: bool,
  /// `size`.
  size: bool,
  /// `has-props`.
  has_properties: bool,
  /// `created-rev`, `time` or `last-author`: any of them reads the entry's commit (`svn_repos_get_committed_info`).
  commit: bool,
}

impl Fields {
  /// Every field: what a request without a list asks for.
  fn all() -> Self {
    Self {
      kind: true,
      size: true,
      has_properties: true,
      commit: true,
    }
  }

  /// `parse_dirent_fields`: the fields a list of words names, other words ignored; `None` for an item that is no word.
  fn of(words: &[Value<'_>]) -> Option<Self> {
    let mut fields = Self::default();
    for word in words {
      let Value::Word(word) = word else {
        return None;
      };
      let [kind, size, has_properties, created, time, author] = DIRENT_FIELDS;
      fields.kind |= *word == kind;
      fields.size |= *word == size;
      fields.has_properties |= *word == has_properties;
      fields.commit |= *word == created || *word == time || *word == author;
    }
    Some(fields)
  }

  /// Whether the entry's commit is read.
  fn reads_commit(self) -> bool {
    self.commit
  }
}

/// A parsed `get-dir` request.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryRequest {
  /// The directory's path.
  path: String,
  /// The revision, the youngest when absent.
  revision: Option<i64>,
  /// Whether its properties are wanted.
  properties: bool,
  /// Whether its entries are wanted.
  contents: bool,
  /// The dirent fields wanted.
  fields: Fields,
  /// Whether its ancestors' properties are wanted.
  inherited: bool,
}

impl DirectoryRequest {
  /// `svn_ra_svn__parse_tuple(params, "c(?r)bb?l?B")`.
  fn parse(params: &[Value<'_>], base: &str) -> Option<Self> {
    let (path, revision) = path_and_revision(params, base)?;
    // Format: `"c(?r)bb?l?B"` is path 0, revision 1, want-props 2, want-contents 3, dirent fields 4
    // and inherited-props 5 (`serve.c` `get_dir`).
    let fields = match params.get(4) {
      None => Fields::all(),
      Some(Value::List(words)) => Fields::of(words)?,
      Some(_) => return None,
    };
    // Format: `"c(?r)bb?l?B"`'s sixth item, inherited-props.
    let inherited = match params.get(5) {
      None => false,
      some => boolean(some)?,
    };
    Some(Self {
      path,
      revision,
      properties: boolean(params.get(2))?,
      // Format: `"c(?r)bb?l?B"`'s want-contents, its fourth item.
      contents: boolean(params.get(3))?,
      fields,
      inherited,
    })
  }
}

/// What `get-dir` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryAnswer<'repository> {
  /// The revision.
  revision: Revision,
  /// The directory's properties, entry properties included; none when not wanted.
  properties: Vec<(Vec<u8>, Vec<u8>)>,
  /// Its entries; none when not wanted.
  entries: Vec<DirectoryEntry<'repository>>,
  /// Its ancestors' properties, when wanted.
  ancestors: Option<OwnedAncestors<'repository>>,
}

/// One entry of `get-dir`'s answer.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryEntry<'repository> {
  /// Its name.
  name: String,
  /// Its kind, when asked.
  kind: Option<NodeKind>,
  /// A file's size, when asked; 0 otherwise.
  size: u64,
  /// Whether it has properties, when asked.
  has_properties: bool,
  /// Its created revision, when its commit was asked; 0 otherwise.
  created: Revision,
  /// That revision's date as stored.
  date: Option<&'repository [u8]>,
  /// That revision's author.
  author: Option<&'repository [u8]>,
}

impl DirectoryEntry<'_> {
  /// `( name kind size has-props created-rev ( date ) ( [ author ] ) )`, the date of time 0 where there is none, as
  /// `get_dir` fills it in for a client that cannot take its absence.
  fn value(&self) -> Value<'_> {
    let date = self
      .date
      .map_or_else(|| Value::String(EPOCH_DATE.as_bytes()), Value::String);
    Value::List(vec![
      Value::String(self.name.as_bytes()),
      Value::Word(self.kind.map_or(NONE_WORD, kind_word)),
      Value::Number(self.size),
      Value::Word(bool_word(self.has_properties)),
      Value::Number(self.created),
      Value::List(vec![date]),
      Value::List(self.author.map(Value::String).into_iter().collect()),
    ])
  }
}

/// `svn_time_to_cstring(0)`, the date `get_dir` writes for an entry without one.
/// Format: `TIMESTAMP_FORMAT` of time 0; recorded (fixture `get-dir`).
const EPOCH_DATE: &str = "1970-01-01T00:00:00.000000Z";

/// `svn_ra_svn__parse_tuple(params, "crl")` for `get-locations`, each revision a number.
fn locations_request(params: &[Value<'_>], base: &str) -> Option<(String, i64, Vec<i64>)> {
  let path = c_string(params.first())?;
  let peg = revnum(params.get(1))?;
  let Some(Value::List(revisions)) = params.get(2) else {
    return None;
  };
  let revisions = revisions
    .iter()
    .map(|revision| revnum(Some(revision)))
    .collect::<Option<Vec<i64>>>()?;
  Some((full_path(base, path), peg, revisions))
}

/// A revision a location names, as a revision number.
fn node_revision_of(revision: i64) -> Result<Revision, Failure> {
  Revision::try_from(revision).map_err(|_| Failure::revision(revision))
}

/// A property list held whole as the protocol writes it, in byte order of the names.
fn owned_proplist(properties: &[(Vec<u8>, Vec<u8>)]) -> Value<'_> {
  proplist(
    properties
      .iter()
      .map(|(name, value)| (name.as_slice(), value.as_slice()))
      .collect(),
  )
}

/// The path of `name` in `directory`.
fn child_path(directory: &str, name: &str) -> String {
  if directory == ROOT {
    format!("{ROOT}{name}")
  } else {
    format!("{directory}{SEPARATOR}{name}")
  }
}

/// A dirent as `stat` writes it, owning its formatted date.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Dirent<'repository> {
  /// The node's kind.
  kind: NodeKind,
  /// Its text's size, or a directory's [`DIRECTORY_SIZE`].
  size: u64,
  /// Whether its property list holds any property.
  has_properties: bool,
  /// The revision it was created in.
  created: Revision,
  /// That revision's date, as `svn_time_to_cstring` writes it.
  date: Option<String>,
  /// That revision's author.
  author: Option<&'repository [u8]>,
}

impl Dirent<'_> {
  /// `( kind size has-props created-rev ( [ date ] ) ( [ author ] ) )`.
  fn value(&self) -> Value<'_> {
    Value::List(vec![
      Value::Word(kind_word(self.kind)),
      Value::Number(self.size),
      Value::Word(bool_word(self.has_properties)),
      Value::Number(self.created),
      Value::List(
        self
          .date
          .iter()
          .map(|date| Value::String(date.as_bytes()))
          .collect(),
      ),
      Value::List(self.author.map(Value::String).into_iter().collect()),
    ])
  }
}

/// A parsed `get-mergeinfo` request.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MergeinfoRequest {
  /// The paths, each the session's base joined with the given path made canonical.
  paths: Vec<String>,
  /// The revision, the youngest when absent.
  revision: Option<i64>,
  /// The inheritance its word names, explicit for a word Subversion does not know.
  inheritance: MergeinfoInheritance,
  /// Whether the mergeinfo beneath each path is wanted.
  descendants: bool,
}

impl MergeinfoRequest {
  /// `svn_ra_svn__parse_tuple(params, "l(?r)wb")`, each path a string ("Path is not a string" is malformed data).
  fn parse(params: &[Value<'_>], base: &str) -> Option<Self> {
    let Some(Value::List(paths)) = params.first() else {
      return None;
    };
    let paths = paths
      .iter()
      .map(|path| match path {
        Value::String(path) => Some(full_path(base, c_string_of(path))),
        _ => None,
      })
      .collect::<Option<Vec<String>>>()?;
    let revision = optional_revnum(params.get(1))?;
    let Some(Value::Word(word)) = params.get(2) else {
      return None;
    };
    Some(Self {
      paths,
      revision,
      inheritance: MergeinfoInheritance::from_word(word.as_bytes()),
      // Format: `"l(?r)wb"`'s fourth item, include-descendants (`serve.c` `get_mergeinfo`).
      descendants: boolean(params.get(3))?,
    })
  }
}

/// A `c` parameter: a string, read as a C string to its first NUL.
fn c_string<'value>(param: Option<&Value<'value>>) -> Option<&'value [u8]> {
  match param {
    Some(Value::String(bytes)) => Some(c_string_of(bytes)),
    _ => None,
  }
}

/// Bytes read as a C string: those before the first NUL.
fn c_string_of(bytes: &[u8]) -> &[u8] {
  bytes
    .split(|&byte| byte == TERMINATOR)
    .next()
    .unwrap_or_default()
}

/// An `r` parameter: a number, cast to a signed long as `svn_ra_svn__parse_tuple` casts it.
fn revnum(param: Option<&Value<'_>>) -> Option<i64> {
  match param {
    Some(Value::Number(number)) => Some(i64::from_ne_bytes(number.to_ne_bytes())),
    _ => None,
  }
}

/// A `(?r)` parameter: a list holding a revision or nothing; malformed when it is no list or holds another kind.
fn optional_revnum(param: Option<&Value<'_>>) -> Option<Option<i64>> {
  let Some(Value::List(items)) = param else {
    return None;
  };
  match items.first() {
    None => Some(None),
    first => revnum(first).map(Some),
  }
}

/// A `b` parameter: the word `true` or `false`.
fn boolean(param: Option<&Value<'_>>) -> Option<bool> {
  match param {
    Some(Value::Word(TRUE_WORD)) => Some(true),
    Some(Value::Word(FALSE_WORD)) => Some(false),
    _ => None,
  }
}

/// A `c(?r)` command's path, joined with the session's base, and its optional revision.
fn path_and_revision(params: &[Value<'_>], base: &str) -> Option<(String, Option<i64>)> {
  let path = c_string(params.first())?;
  let revision = optional_revnum(params.get(1))?;
  Some((full_path(base, path), revision))
}

/// The session's base joined with a relative path made canonical (`svn_relpath_canonicalize_safe`,
/// `svn_fspath__join`): empty and `.` names dropped, every other name kept, `..` among them.
pub(crate) fn full_path(base: &str, relative: &[u8]) -> String {
  let relative = String::from_utf8_lossy(relative);
  let names: Vec<&str> = relative
    .split(SEPARATOR)
    .filter(|name| !name.is_empty() && *name != CURRENT_DIRECTORY)
    .collect();
  let joined = names.join("/");
  if joined.is_empty() {
    base.to_owned()
  } else if base == ROOT {
    format!("{ROOT}{joined}")
  } else {
    format!("{base}{SEPARATOR}{joined}")
  }
}

/// A path relative to the session's base (`svn_fspath__skip_ancestor`): empty for the base itself.
fn relative_to(base: &str, path: &str) -> String {
  if base == ROOT {
    return path.strip_prefix(SEPARATOR).unwrap_or(path).to_owned();
  }
  match path.strip_prefix(base) {
    Some(rest) => rest.strip_prefix(SEPARATOR).unwrap_or(rest).to_owned(),
    None => path.to_owned(),
  }
}

/// The parent of a path other than the root.
fn parent(path: &str) -> Option<&str> {
  if path == ROOT {
    return None;
  }
  match path.rfind(SEPARATOR) {
    Some(0) => Some(ROOT),
    Some(cut) => path.get(..cut),
    None => None,
  }
}

/// A property list as the protocol writes it, `( ( name value ) ... )`, in byte order of the names.
fn proplist<'value>(mut properties: Vec<(&'value [u8], &'value [u8])>) -> Value<'value> {
  properties.sort_unstable();
  Value::List(
    properties
      .into_iter()
      .map(|(name, value)| Value::List(vec![Value::String(name), Value::String(value)]))
      .collect(),
  )
}

/// A node kind's word.
fn kind_word(kind: NodeKind) -> &'static str {
  match kind {
    NodeKind::File => FILE_WORD,
    NodeKind::Directory => DIR_WORD,
  }
}

/// A boolean's word.
fn bool_word(value: bool) -> &'static str {
  if value { TRUE_WORD } else { FALSE_WORD }
}

/// One field of a date as `svn__strtoul` reads it (decimal digits only, their value wrapping as an unsigned long's),
/// cast to 32 bits as `svn_time_from_cstring` casts it; and the place after the digits.
fn time_field(text: &[u8], at: usize) -> (i32, usize) {
  let digits: &[u8] = text
    .get(at..)
    .unwrap_or_default()
    .split(|byte| !byte.is_ascii_digit())
    .next()
    .unwrap_or_default();
  let value = digits.iter().fold(0_u64, |value, &digit| {
    value
      .wrapping_mul(DECIMAL)
      .wrapping_add(u64::from(digit.wrapping_sub(b'0')))
  });
  let low = u32::try_from(value & u64::from(u32::MAX)).unwrap_or_default();
  (
    i32::from_ne_bytes(low.to_ne_bytes()),
    at.saturating_add(digits.len()),
  )
}

/// `svn_time_from_cstring`'s format, `YYYY-MM-DDTHH:MM:SS.UUUUUUZ`, each field followed by its separator, then APR's
/// `apr_time_exp_gmt_get`: microseconds since 1970, or `None` for a bogus date.
fn parse_time(text: &[u8]) -> Option<i64> {
  let mut fields = [0_i32; TIME_SEPARATORS.len()];
  let mut at = 0;
  for (field, separator) in fields.iter_mut().zip(TIME_SEPARATORS) {
    let (value, after) = time_field(text, at);
    if text.get(after) != Some(&separator) {
      return None;
    }
    *field = value;
    at = after.saturating_add(1);
  }
  let [year, month, day, hour, minute, second, microsecond] = fields;
  time_from_fields(
    i64::from(year.wrapping_sub(TM_YEAR_BASE)),
    i64::from(month.wrapping_sub(1)),
    [day, hour, minute, second, microsecond].map(i64::from),
  )
}

/// APR's `apr_time_exp_get` with no offset: refused for a month outside 0-11 or a time before 1970; every other field
/// taken as it is, its arithmetic C's.
fn time_from_fields(
  tm_year: i64,
  tm_month: i64,
  [day, hour, minute, second, microsecond]: [i64; 5],
) -> Option<i64> {
  let offset = DAY_OFFSET.get(usize::try_from(tm_month).ok()?).copied()?;
  let year = if tm_month < MARCH {
    tm_year.wrapping_sub(1)
  } else {
    tm_year
  };
  let centuries = year.checked_div(CENTURY)?;
  let days = year
    .wrapping_mul(YEAR_DAYS)
    .wrapping_add(year.checked_div(LEAP_YEARS)?)
    .wrapping_sub(centuries)
    .wrapping_add(
      centuries
        .wrapping_add(CENTURY_OFFSET)
        .checked_div(LEAP_YEARS)?,
    )
    .wrapping_add(offset)
    .wrapping_add(day)
    .wrapping_sub(1)
    .wrapping_sub(EPOCH_FROM_1900_MARCH);
  let seconds = days
    .wrapping_mul(HOURS)
    .wrapping_add(hour)
    .wrapping_mul(SIXTY)
    .wrapping_add(minute)
    .wrapping_mul(SIXTY)
    .wrapping_add(second);
  (seconds >= 0).then(|| seconds.wrapping_mul(MICROSECONDS).wrapping_add(microsecond))
}

/// `svn_time_to_cstring`: a time as `%04d-%02d-%02dT%02d:%02d:%02d.%06dZ`, by APR's `apr_time_exp_gmt` (seconds by C's
/// truncating division, the day and its time by `gmtime`'s floor).
fn format_time(time: i64) -> String {
  let seconds = time.checked_div(MICROSECONDS).unwrap_or_default();
  let microsecond = time.checked_rem(MICROSECONDS).unwrap_or_default();
  let days = seconds.div_euclid(DAY_SECONDS);
  let in_day = seconds.rem_euclid(DAY_SECONDS);
  let (year, month, day) = civil_from_days(days);
  let hour = in_day.div_euclid(SIXTY.wrapping_mul(SIXTY));
  let minute = in_day.div_euclid(SIXTY).rem_euclid(SIXTY);
  let second = in_day.rem_euclid(SIXTY);
  format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{microsecond:06}Z")
}

/// The Gregorian date of a day counted from 1970-01-01 (Howard Hinnant's `civil_from_days`, which `gmtime` computes).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
  let [four_years, century, era_last] = ERA_CORRECTIONS;
  let shifted = days.wrapping_add(EPOCH_FROM_YEAR_ZERO_MARCH);
  let era = shifted.div_euclid(ERA_DAYS);
  let day_of_era = shifted.rem_euclid(ERA_DAYS);
  let year_of_era = day_of_era
    .wrapping_sub(day_of_era.div_euclid(four_years))
    .wrapping_add(day_of_era.div_euclid(century))
    .wrapping_sub(day_of_era.div_euclid(era_last))
    .div_euclid(YEAR_DAYS);
  let day_of_year = day_of_era.wrapping_sub(
    YEAR_DAYS
      .wrapping_mul(year_of_era)
      .wrapping_add(year_of_era.div_euclid(LEAP_YEARS))
      .wrapping_sub(year_of_era.div_euclid(CENTURY)),
  );
  let month_index = FIVE_MONTHS
    .wrapping_mul(day_of_year)
    .wrapping_add(MONTH_ROUNDING)
    .div_euclid(FIVE_MONTH_DAYS);
  let day = day_of_year
    .wrapping_sub(
      FIVE_MONTH_DAYS
        .wrapping_mul(month_index)
        .wrapping_add(MONTH_ROUNDING)
        .div_euclid(FIVE_MONTHS),
    )
    .wrapping_add(1);
  let month = if month_index < MARCH_TO_DECEMBER {
    month_index.wrapping_add(MARCH_NUMBER)
  } else {
    month_index.wrapping_sub(WRAP_MONTHS)
  };
  let year = year_of_era
    .wrapping_add(era.wrapping_mul(ERA_YEARS))
    .wrapping_add(i64::from(month <= FEBRUARY));
  (year, month, day)
}

mod deleted;
mod file;
mod file_revs;
mod fold;
mod history;
mod list;
pub(crate) mod locks;
mod log;
mod segments;
mod stream;

pub use file_revs::DavFileRevs;

/// Where the node at `path` in `peg` stood in each of `revisions`, as `svn_repos_trace_node_locations` traces it, read
/// from `index` and `repository` with each path judged by `rules` (none: every path readable): the `get-locations`
/// report's walk over HTTP (`reports/get-locations.c`; A-726), the svn:// command's own (`get-locations`). Each found
/// location as its revision and its path, in the trace's order; or Subversion's error number and message.
///
/// # Errors
/// The trace's refusal: a revision past the youngest, an unreadable path, a path missing in the peg revision.
pub fn trace_node_locations<R: Repository + ?Sized>(
  index: &Index,
  repository: &R,
  rules: Option<&PrincipalRules>,
  (path, peg): (&str, Revision),
  revisions: &[Option<Revision>],
) -> Result<Vec<(Revision, String)>, (u32, String)> {
  let mut out = Vec::new();
  let reads = Reads {
    index,
    repository,
    base: ROOT,
    uuid: "",
    authz: rules,
    authentication: Authentication::Settled,
    out: &mut out,
  };
  let signed = |revision: Revision| i64::try_from(revision).unwrap_or(i64::MAX);
  let peg = signed(peg);
  let revisions = revisions
    .iter()
    .map(|revision| revision.map_or(INVALID_REVISION, signed))
    .collect();
  reads
    .trace_locations(path, peg, revisions)
    .map_err(|failure| (failure.code, failure.message))
}

/// The mergeinfo catalog of `paths` (repository paths) at `revision` (the youngest when `None`), as
/// `svn_repos_fs_get_mergeinfo2` gathers it, read from `index` and `repository` with each path judged by `rules` (none:
/// every path readable): the `mergeinfo-report`'s over HTTP (`reports/mergeinfo.c`; A-726 part 2), the svn://
/// command's own (`get-mergeinfo`). Each entry as its path relative to `target` (`svn_fspath__skip_ancestor`) and its
/// mergeinfo's string; or Subversion's error number and message.
///
/// # Errors
/// The walk's refusal: a revision past the youngest, a path missing in it.
pub fn repository_mergeinfo<R: Repository + ?Sized>(
  index: &Index,
  repository: &R,
  rules: Option<&PrincipalRules>,
  (target, paths): (&str, Vec<String>),
  revision: Option<Revision>,
  (inheritance, descendants): (mergeinfo::MergeinfoInheritance, bool),
) -> Result<Vec<(String, String)>, (u32, String)> {
  let mut out = Vec::new();
  let reads = Reads {
    index,
    repository,
    base: ROOT,
    uuid: "",
    authz: rules,
    authentication: Authentication::Settled,
    out: &mut out,
  };
  let request = MergeinfoRequest {
    paths,
    revision: revision.map(|revision| i64::try_from(revision).unwrap_or(i64::MAX)),
    inheritance,
    descendants,
  };
  reads
    .mergeinfo_catalog(&request)
    .map(|catalog| {
      catalog
        .into_iter()
        .map(|(path, value)| {
          (
            relative_to(target, &path),
            String::from_utf8_lossy(&value).into_owned(),
          )
        })
        .collect()
    })
    .map_err(|failure| (failure.code, failure.message))
}

/// `relative`, a report's `S:path`, joined to `target`, the repository path of the resource the report names, as
/// `svn_fspath__join` joins them.
pub fn full_path_of(target: &str, relative: &str) -> String {
  full_path(target, relative.as_bytes())
}

/// `SVN_INVALID_REVNUM`: a location revision the body wrote negative.
/// Format: `svn_types.h`, -1.
const INVALID_REVISION: i64 = -1;
pub use log::dav::{DavLog, DavLogProgress};
pub use stream::{Progress, Stream};

#[cfg(test)]
mod tests;
