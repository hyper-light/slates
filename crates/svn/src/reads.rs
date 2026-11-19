//! The ra_svn read commands (§4.17.4 "4. Reads" and "5. The wire", piece 7's second part; A-613): a sans-io handler
//! that answers one parsed command of a session after its handshake, from the Subversion index ([`crate::index`],
//! A-553), the mergeinfo queries ([`crate::mergeinfo`], A-605) and the green's revision properties and property lists
//! ([`Repository`]), writing exactly what `svnserve` writes.
//!
//! Every answer is `serve.c`'s (Subversion `1.14.x` at `d434dc14`; the deployed binaries stock 1.15.0), held to
//! transcripts of stock `svnserve` answering stock `svn` and scripted clients (`crates/svn/fixtures/reads`, each file's
//! SHA-256 in `SHA256SUMS`):
//! - **The order of a command's writes.** A command whose path is checked for read access (`check-path`, `stat`,
//!   `get-iprops`) or that runs at all writes the trivial authentication request first, `( success ( ( ) 0: ) )`
//!   (`trivial_auth_request`, the protocol's §2 "auth-request" with no mechanism), then its answer or its refusal; so a
//!   revision past the youngest is refused after it.
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
//! (`svn_repos_fs_get_mergeinfo2`) and `get-iprops` (`svn_repos_fs_get_inherited_props`). Owed with this piece:
//! `get-file`, `get-dir`, `list`, `log` with merged revisions, `get-locations`, `get-location-segments`,
//! `get-file-revs`, `get-deleted-rev`, `svn mergeinfo --show-revs`, and read access by the authorization rules (A-606):
//! today every path is readable, as in an `anon-access = read` repository with no rules.
//!
//! Bounds: an answer holds what its command names, bounded by the index and the green (a property list, a revision's
//! properties, a request's paths and their mergeinfo); the streaming commands that follow write a slice at a time.
//!
//! No I/O, no clock, no randomness, no panic.

use crate::index::{ContentIdentity, Index, IndexRefusal, Node, NodeKind, PropertyList, Revision};
use crate::mergeinfo::{self, Contents, MergeinfoInheritance, QueryRefusal};
use crate::ra_svn::{ItemError, Value, write};

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
const ELSEWHERE: [&str; 24] = [
  "reparent",
  "change-rev-prop",
  "change-rev-prop2",
  "commit",
  "get-file",
  "get-dir",
  "update",
  "switch",
  "status",
  "diff",
  "log",
  "get-locations",
  "get-location-segments",
  "get-file-revs",
  "lock",
  "lock-many",
  "unlock",
  "unlock-many",
  "get-lock",
  "get-locks",
  "replay",
  "replay-range",
  "get-deleted-rev",
  "list",
];

/// The separators of `svn_time_from_cstring`'s format, each after its field: year, month, day, hour, minute, second,
/// microsecond.
/// Format: `TIMESTAMP_FORMAT`, `%04d-%02d-%02dT%02d:%02d:%02d.%06dZ` (`time.c`).
const TIME_SEPARATORS: [u8; 7] = [b'-', b'-', b'T', b':', b':', b'.', b'Z'];

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

/// The session a command runs in, as the handshake (A-599) hands it over once the client is authenticated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session<'session> {
  /// The repository's path the session's URL names, canonical (`svnserve`'s `fs_path`): every path a command names is
  /// beneath it, and the paths it answers with are relative to it.
  pub base: &'session str,
}

/// What the read commands read from the green beyond the index and the mergeinfo queries' [`Contents`]: revision
/// properties (§4.17.4 item 6) and property lists (item 1).
pub trait Repository: Contents {
  /// Revision `revision`'s properties, every one, in any order; `None` for a revision the green does not hold.
  fn revision_properties(&self, revision: Revision) -> Option<Vec<(&[u8], &[u8])>>;

  /// The property list held as `identity`, every property, in any order; `None` when the green does not hold it.
  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>>;
}

/// What the handler made of a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
  /// Answered: its answer, or its refusal, is appended to the output.
  Answered,
  /// Malformed: not a command, or parameters `svn_ra_svn__parse_tuple` refuses (`SVN_ERR_RA_SVN_MALFORMED_DATA`), which
  /// `svnserve` answers by ending the connection without a word. Nothing is appended; the caller ends the connection.
  Close,
  /// A command of the main set another piece answers. Nothing is appended.
  Elsewhere,
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
    other if ELSEWHERE.contains(&other) => Ok(Outcome::Elsewhere),
    other => reads.unknown(other),
  }
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
      if this > time {
        let Some(previous) = middle.checked_sub(1) else {
          return Ok(0);
        };
        if self.time_of(previous)? <= time {
          return Ok(previous);
        }
        top = previous;
      } else if this < time {
        let next = middle.saturating_add(1);
        if next > latest {
          return Ok(latest);
        }
        if self.time_of(next)? > time {
          return Ok(middle);
        }
        bottom = next;
      } else {
        return Ok(middle);
      }
    }
    Ok(0)
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

  /// `rev-proplist ( rev:number )`: a revision's properties.
  fn revision_list(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some(revision) = revnum(params.first()) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
    match self.existing(revision) {
      Ok(revision) => {
        let properties = self
          .repository
          .revision_properties(revision)
          .unwrap_or_default();
        self.success(vec![proplist(properties)])
      }
      Err(failure) => self.failure(&failure),
    }
  }

  /// `rev-prop ( rev:number name:string )`: one revision property, or none.
  fn revision_property_command(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let (Some(revision), Some(name)) = (revnum(params.first()), c_string(params.get(1))) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
    match self.existing(revision) {
      Ok(revision) => {
        let value = self.revision_property(revision, name);
        self.success(vec![Value::List(
          value.map(Value::String).into_iter().collect(),
        )])
      }
      Err(failure) => self.failure(&failure),
    }
  }

  /// `check-path ( path:string [ rev:number ] )`: the kind of node at the path, `none` where there is none.
  fn check_path(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some((path, revision)) = path_and_revision(params, self.base) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
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

  /// `stat ( path:string [ rev:number ] )`: the node's dirent (`svn_repos_stat`), or nothing where there is none.
  fn stat(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some((path, revision)) = path_and_revision(params, self.base) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
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
    let found = self.at_or_youngest(request.revision).and_then(|revision| {
      let paths: Vec<&str> = request.paths.iter().map(String::as_str).collect();
      mergeinfo::catalog(
        self.index,
        self.repository,
        &paths,
        revision,
        request.inheritance,
        request.descendants,
      )
      .map_err(|refusal| Failure::of_query(&refusal, revision))
    });
    match found {
      Ok(catalog) => {
        let entries: Vec<(String, Vec<u8>)> = catalog
          .into_iter()
          .map(|(path, mergeinfo)| (relative_to(self.base, &path), mergeinfo.to_bytes()))
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

  /// `get-iprops ( path:string [ rev:number ] )`: the property lists of the path's ancestors that have any, the root's
  /// first (`svn_repos_fs_get_inherited_props`). An absent revision is not the youngest here: `svnserve` opens the
  /// root of the invalid revision, and so refuses it.
  fn inherited(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some((path, revision)) = path_and_revision(params, self.base) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
    let found = self
      .existing(revision.unwrap_or(-1))
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

  /// The ancestors of `path` holding properties, root first, each by its path without the leading separator.
  #[allow(clippy::type_complexity)]
  fn ancestors_properties<'path>(
    &self,
    path: &'path str,
    revision: Revision,
  ) -> Result<Vec<(&'path str, Vec<(&'reads [u8], &'reads [u8])>)>, Failure> {
    if self.node(path, revision)?.is_none() {
      return Err(Failure::new(
        SVN_ERR_FS_NOT_FOUND,
        format!("'{path}' path not found"),
      ));
    }
    let mut found = Vec::new();
    let mut place = parent(path);
    while let Some(ancestor) = place {
      if let Some(node) = self.node(ancestor, revision)?
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
    let error = Value::List(vec![
      Value::Number(u64::from(failure.code)),
      Value::String(failure.message.as_bytes()),
      Value::String(b""),
      Value::Number(0),
    ]);
    write(
      &Value::List(vec![Value::Word(FAILURE), Value::List(vec![error])]),
      self.out,
    )?;
    Ok(Outcome::Answered)
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
fn full_path(base: &str, relative: &[u8]) -> String {
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
  let mut fields = [0_i32; 7];
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

#[cfg(test)]
mod tests;
