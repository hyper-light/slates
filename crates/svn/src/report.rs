//! The reporter's commands and the report a client gives of its working copy, the server's side
//! (§4.17.4 "4. Reads", the reporter, and piece 7's third part; A-616, its first part): `update`,
//! `switch`, `status` and `diff` read as `svnserve` reads them; then `set-path`, `delete-path`,
//! `link-path`, `finish-report` and `abort-report` kept as `svnserve`'s reporter keeps them, with
//! every refusal it makes, until the report is finished and handed to the editor drive that answers
//! it.
//!
//! What `svnserve` does, read in its code (Subversion's `1.14.x` at `d434dc14`; 1.15.0's
//! `reporter.c` differs by two comments and a hash constructor, its `serve.c` by a Windows
//! include):
//! 1. The command's parameters (`serve.c` `update` 1948, `switch_cmd` 2008, `status` 2064 and
//!    `diff` 2105), read through `vparse_tuple` ([`tuple`]): `update` `(?r)cb?w3?3`, `switch`
//!    `(?r)cbc?w?33`, `status` `cb?(?r)?w`, `diff` `(?r)cbbc` from a client before 1.4 and
//!    `(?r)cbbcb?w` after. Parameters that do not match end the connection. A depth word Subversion
//!    does not know is `unknown`; with no word, `recurse` decides. `update` first asks whether the
//!    principal may read its target, and is refused 170001 "Authorization failed" if not; the
//!    others answer a trivial auth request at once. `switch` and `diff` name a URL, refused 170000
//!    when it is not the session's repository's. A requested depth of `exclude` is refused 165002.
//!    Each refusal is a command failure, after which the connection reads its next command.
//! 2. The report (`serve.c` `set_path`, `delete_path`, `link_path`, `finish_report`,
//!    `abort_report`; `reporter.c` `write_path_info`): every path made anchor-relative, joined to
//!    the command's target. The first refusal is kept and the rest of the report read and dropped:
//!    a depth `unknown` 165002 "Unsupported report depth 'unknown'", a link at depth `exclude`
//!    165002, a link URL outside the repository 170000. A command the report does not know is
//!    answered 210001 "Unknown editor command" at once, and the report goes on. `finish-report`
//!    answers a trivial auth request, then the kept refusal, or checks the report's top level as
//!    `finish_report` does: its first path is a `set-path` of the target (165004 "Invalid report
//!    for top level of working copy"), and a second report of the target is allowed only below the
//!    anchor (165004 "Two top-level reports with no target"); a deletion there takes the first
//!    report's depth. `abort-report` answers `( success ( ) )`.
//!
//! Evidence: stock Subversion 1.15.0's own sessions, `svn` against `svnserve` with every byte each
//! way logged by a relay (`crates/svn/fixtures/report/`, `make-fixtures.py` over `history.txt`,
//! `SHA256SUMS`): every stock command's report, and hand-made reports for each refusal above.
//!
//! **Stricter than `svnserve`**, on hostile input alone: a path, URL or token holding a NUL byte,
//! which `svnserve` reads as a C string cut at the NUL, refuses the command; a path or decoded URL
//! that is no UTF-8 refuses it, since no repository path is such; and a URL that has the
//! repository's URL as a prefix without a `/` after it (`svn://host/repo2` beside
//! `svn://host/repo`) is not the repository's, where `get_fs_path`'s plain prefix test takes it.
//!
//! **Bounds.** One report command's item within the caller's bytes; the longest word a stock parser
//! takes; the deepest list a report command holds, 3; the item count the bytes over what a parsed
//! item costs (the handshake's rule). The report itself is held within the caller's room: past it,
//! the connection is refused 210009, as `svnserve` refuses a request past its size.
//!
//! Owed (GAPS): URLs read as `svn_uri_canonicalize` reads them, where a stock client's are
//! canonical already; and the authentication `svnserve` asks for when an anonymous `update` may not
//! read but a user could. No I/O, no clock, no randomness, no panic.

pub mod drive;
pub mod editor;
pub mod exchange;
#[cfg(test)]
pub(crate) mod recorded;
mod tuple;

use crate::handshake::{self, Session};
use crate::index::Revision;
use crate::ra_svn::{self, Bounds, ItemError, Parsed, Parser, Value};
use tuple::Field;

/// The word of the command that brings a working copy to a revision.
/// Format: `serve.c`'s `main_commands`.
const UPDATE: &str = "update";

/// The word of the command that brings a working copy to another path.
/// Format: `serve.c`'s `main_commands`.
const SWITCH: &str = "switch";

/// The word of the command that tells a working copy what changed, with no texts.
/// Format: `serve.c`'s `main_commands`.
const STATUS: &str = "status";

/// The word of the command that compares a working copy's report with a URL.
/// Format: `serve.c`'s `main_commands`.
const DIFF: &str = "diff";

/// The word of the report command that reports a path at a revision.
/// Format: `serve.c`'s `report_commands`.
const SET_PATH: &str = "set-path";

/// The word of the report command that reports a path missing.
/// Format: `serve.c`'s `report_commands`.
const DELETE_PATH: &str = "delete-path";

/// The word of the report command that reports a path switched to another.
/// Format: `serve.c`'s `report_commands`.
const LINK_PATH: &str = "link-path";

/// The word of the report command that ends the report.
/// Format: `serve.c`'s `report_commands`.
const FINISH_REPORT: &str = "finish-report";

/// The word of the report command that abandons the report.
/// Format: `serve.c`'s `report_commands`.
const ABORT_REPORT: &str = "abort-report";

/// The word a command response starts with.
/// Format: `protocol` §1's `command-response`.
const SUCCESS: &str = "success";

/// `update`'s parameters.
/// Format: `serve.c` 1963.
const UPDATE_FORMAT: &[u8] = b"(?r)cb?w3?3";

/// `switch`'s parameters.
/// Format: `serve.c` 2023.
const SWITCH_FORMAT: &[u8] = b"(?r)cbc?w?33";

/// `status`'s parameters.
/// Format: `serve.c` 2077.
const STATUS_FORMAT: &[u8] = b"cb?(?r)?w";

/// `diff`'s parameters from a client before 1.4, which sends five.
/// Format: `serve.c` 2121.
const OLD_DIFF_FORMAT: &[u8] = b"(?r)cbbc";

/// The count of `diff`'s parameters from a client before 1.4.
/// Format: `serve.c` 2118, `params->nelts == 5`.
const OLD_DIFF_PARAMETERS: usize = 5;

/// `diff`'s parameters.
/// Format: `serve.c` 2129.
const DIFF_FORMAT: &[u8] = b"(?r)cbbcb?w";

/// `set-path`'s parameters.
/// Format: `serve.c` 869.
const SET_PATH_FORMAT: &[u8] = b"crb?(?c)?w";

/// `delete-path`'s parameters.
/// Format: `serve.c` 894.
const DELETE_PATH_FORMAT: &[u8] = b"c";

/// `link-path`'s parameters.
/// Format: `serve.c` 915.
const LINK_PATH_FORMAT: &[u8] = b"ccrb?(?c)?w";

/// A command's own shape.
/// Format: `marshal.c` `svn_ra_svn__handle_command` reads `wl`.
const COMMAND_FORMAT: &[u8] = b"wl";

/// The deepest list a report command holds: the command, its parameters, its lock token's list.
/// Format: `( set-path ( path rev start-empty ( token ) depth ) )`, `protocol` §3.1.3.
const REPORT_DEPTH: usize = 3;

/// The longest word read.
/// Format: `marshal.c`'s `MAX_WORD_LENGTH` is 25 with the terminating byte (the handshake's bound).
const LONGEST_WORD: usize = 24;

/// What one parsed item costs in memory, a [`Value`] whatever its kind.
/// Derived: `size_of::<Value>()`, measured by the compiler for the target (the handshake's rule).
const PARSED_ITEM_BYTES: usize = size_of::<Value<'static>>();

/// What one kept path costs the report beyond its strings' bytes.
/// Derived: `size_of::<PathInfo>()`, measured by the compiler for the target.
const PATH_INFO_BYTES: usize = size_of::<PathInfo>();

/// The separator of a path's names.
/// Format: Subversion's paths (`svn_relpath_join`, `svn_fspath__join`).
const SEPARATOR: char = '/';

/// The name that names the directory itself, which a canonical path drops.
/// Format: `dirent_uri.c` `canonicalize`.
const CURRENT_DIRECTORY: &str = ".";

/// The byte that starts a URL's escaped byte.
/// Format: `path.c` `svn_path_uri_decode`.
const ESCAPE: u8 = b'%';

/// The byte that starts a URL's query, after which `+` is a space.
/// Format: `path.c` `svn_path_uri_decode`.
const QUERY: u8 = b'?';

/// The byte a query writes a space as.
/// Format: `path.c` `svn_path_uri_decode` (RFC 2396 §3.3).
const QUERY_SPACE: u8 = b'+';

/// The bits a hexadecimal digit stands for.
/// Format: one digit is four bits.
const HEX_BITS: u32 = 4;

/// The base of a hexadecimal digit's value.
/// Format: `strtol(digits, NULL, 16)` in `svn_path_uri_decode`.
const HEX_RADIX: u32 = 16;

/// `SVN_ERR_REPOS_BAD_ARGS`. Format: the repos category's start, 120,000 + 9 × 5,000 = 165,000,
/// plus 2 (`svn_error_codes.h` 916).
const SVN_ERR_REPOS_BAD_ARGS: u32 = 165_002;

/// `SVN_ERR_REPOS_BAD_REVISION_REPORT`.
/// Format: the repos category's start plus 4 (`svn_error_codes.h` 924).
const SVN_ERR_REPOS_BAD_REVISION_REPORT: u32 = 165_004;

/// `SVN_ERR_RA_ILLEGAL_URL`. Format: the RA category's start, 120,000 + 10 × 5,000 = 170,000, plus
/// 0 (`svn_error_codes.h` 962).
const SVN_ERR_RA_ILLEGAL_URL: u32 = 170_000;

/// `SVN_ERR_RA_NOT_AUTHORIZED`, whose default message `must_have_access` sends.
/// Format: the RA category's start plus 1 (`svn_error_codes.h` 966).
const SVN_ERR_RA_NOT_AUTHORIZED: u32 = 170_001;

/// `SVN_ERR_RA_SVN_UNKNOWN_CMD`. Format: the ra_svn category's start, 120,000 + 18 × 5,000 =
/// 210,000, plus 1 (`svn_error_codes.h`).
const SVN_ERR_RA_SVN_UNKNOWN_CMD: u32 = 210_001;

/// `SVN_ERR_RA_SVN_MALFORMED_DATA`.
/// Format: the ra_svn category's start plus 4.
const SVN_ERR_RA_SVN_MALFORMED_DATA: u32 = 210_004;

/// `SVN_ERR_RA_SVN_REQUEST_SIZE`.
/// Format: the ra_svn category's start plus 9 (`svn_error_codes.h` 1601).
const SVN_ERR_RA_SVN_REQUEST_SIZE: u32 = 210_009;

/// A depth, ordered as `svn_depth_t` orders them: `unknown` lowest, then `exclude`, `empty`,
/// `files`, `immediates` and `infinity` (`svn_types.h`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Depth {
  /// No depth asked: the report's own depths decide.
  Unknown,
  /// The path is excluded from the working copy.
  Exclude,
  /// The path alone.
  Empty,
  /// The path and the files directly in it.
  Files,
  /// The path and every entry directly in it, directories empty.
  Immediates,
  /// Everything beneath.
  Infinity,
}

impl Depth {
  /// The depth a word names, `unknown` for a word Subversion does not know (`svn_depth_from_word`).
  pub fn from_word(word: &str) -> Self {
    match word {
      "exclude" => Self::Exclude,
      "empty" => Self::Empty,
      "files" => Self::Files,
      "immediates" => Self::Immediates,
      "infinity" => Self::Infinity,
      _ => Self::Unknown,
    }
  }

  /// The depth's word (`svn_depth_to_word`).
  pub fn word(self) -> &'static str {
    match self {
      Self::Unknown => "unknown",
      Self::Exclude => "exclude",
      Self::Empty => "empty",
      Self::Files => "files",
      Self::Immediates => "immediates",
      Self::Infinity => "infinity",
    }
  }
}

/// The command a report answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
  /// `update`: the working copy to a revision.
  Update,
  /// `switch`: the working copy to another path.
  Switch,
  /// `status`: what changed, with no texts.
  Status,
  /// `diff`: the working copy's report against a URL.
  Diff,
}

impl RequestKind {
  /// The command a word names, if it begins a report.
  pub fn of(word: &str) -> Option<Self> {
    match word {
      UPDATE => Some(Self::Update),
      SWITCH => Some(Self::Switch),
      STATUS => Some(Self::Status),
      DIFF => Some(Self::Diff),
      _ => None,
    }
  }
}

/// A command that begins a report, read as `svnserve` reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
  /// Which command.
  pub kind: RequestKind,
  /// The revision asked for; none for the youngest.
  pub revision: Option<Revision>,
  /// The working copy's target beneath the session's path, a canonical relative path, `""` for the
  /// session's path itself.
  pub target: String,
  /// The depth asked for, `unknown` where the report's own depths decide.
  pub depth: Depth,
  /// `switch`'s URL or `diff`'s, as the client sent it.
  pub url: Option<String>,
  /// Whether file texts are sent (not for `status`).
  pub text_deltas: bool,
  /// Whether an added file names what it was copied from.
  pub send_copyfrom_args: bool,
  /// Whether two nodes of one kind are related whatever their history.
  pub ignore_ancestry: bool,
}

impl Request {
  /// Reads the parameters of the command `kind` names.
  ///
  /// # Errors
  /// [`Refused::Malformed`] for parameters `svnserve`'s `vparse_tuple` refuses, and for a path or
  /// URL holding a NUL byte or no UTF-8; the connection ends.
  pub fn read(kind: RequestKind, params: &[Value<'_>]) -> Result<Self, Refused> {
    match kind {
      RequestKind::Update => read_update(params),
      RequestKind::Switch => read_switch(params),
      RequestKind::Status => read_status(params),
      RequestKind::Diff => read_diff(params),
    }
  }
}

/// `update`'s parameters: `(?r)cb?w3?3`.
fn read_update(params: &[Value<'_>]) -> Result<Request, Refused> {
  let fields = tuple::read(params, UPDATE_FORMAT).map_err(|_| Refused::Malformed)?;
  let [
    Field::Revision(revision),
    Field::String(Some(target)),
    Field::Boolean(recurse),
    Field::Word(depth),
    Field::Tristate(copyfrom),
    Field::Tristate(ancestry),
  ] = fields.as_slice()
  else {
    return Err(Refused::Malformed);
  };
  Ok(Request {
    kind: RequestKind::Update,
    revision: *revision,
    target: canonical_relpath(text(target)?),
    depth: requested_depth(*depth, *recurse, Depth::Files),
    url: None,
    text_deltas: true,
    send_copyfrom_args: *copyfrom == Some(true),
    ignore_ancestry: *ancestry == Some(true),
  })
}

/// `switch`'s parameters: `(?r)cbc?w?33`; ancestry is ignored unless the client says false.
fn read_switch(params: &[Value<'_>]) -> Result<Request, Refused> {
  let fields = tuple::read(params, SWITCH_FORMAT).map_err(|_| Refused::Malformed)?;
  let [
    Field::Revision(revision),
    Field::String(Some(target)),
    Field::Boolean(recurse),
    Field::String(Some(url)),
    Field::Word(depth),
    Field::Tristate(copyfrom),
    Field::Tristate(ancestry),
  ] = fields.as_slice()
  else {
    return Err(Refused::Malformed);
  };
  Ok(Request {
    kind: RequestKind::Switch,
    revision: *revision,
    target: canonical_relpath(text(target)?),
    depth: requested_depth(*depth, *recurse, Depth::Files),
    url: Some(text(url)?.to_owned()),
    text_deltas: true,
    send_copyfrom_args: *copyfrom == Some(true),
    ignore_ancestry: *ancestry != Some(false),
  })
}

/// `status`'s parameters: `cb?(?r)?w`; with no depth word, `recurse` false is depth empty.
fn read_status(params: &[Value<'_>]) -> Result<Request, Refused> {
  let fields = tuple::read(params, STATUS_FORMAT).map_err(|_| Refused::Malformed)?;
  let [
    Field::String(Some(target)),
    Field::Boolean(recurse),
    Field::Revision(revision),
    Field::Word(depth),
  ] = fields.as_slice()
  else {
    return Err(Refused::Malformed);
  };
  Ok(Request {
    kind: RequestKind::Status,
    revision: *revision,
    target: canonical_relpath(text(target)?),
    depth: requested_depth(*depth, *recurse, Depth::Empty),
    url: None,
    text_deltas: false,
    send_copyfrom_args: false,
    ignore_ancestry: false,
  })
}

/// `diff`'s fields: revision, target, recurse, ancestry, URL, texts and depth word; a client before
/// 1.4 sends no texts flag (texts are sent) and no depth word.
type DiffFields<'a> = (
  Option<Revision>,
  &'a [u8],
  bool,
  bool,
  &'a [u8],
  bool,
  Option<&'a str>,
);

/// `diff`'s parameters: five from a client before 1.4 (`(?r)cbbc`), else `(?r)cbbcb?w`.
fn read_diff(params: &[Value<'_>]) -> Result<Request, Refused> {
  let old = params.len() == OLD_DIFF_PARAMETERS;
  let format = if old { OLD_DIFF_FORMAT } else { DIFF_FORMAT };
  let fields = tuple::read(params, format).map_err(|_| Refused::Malformed)?;
  let (revision, target, recurse, ancestry, url, text_deltas, depth) =
    diff_fields(&fields).ok_or(Refused::Malformed)?;
  Ok(Request {
    kind: RequestKind::Diff,
    revision,
    target: canonical_relpath(text(target)?),
    depth: requested_depth(depth, recurse, Depth::Files),
    url: Some(text(url)?.to_owned()),
    text_deltas,
    send_copyfrom_args: false,
    ignore_ancestry: ancestry,
  })
}

/// `diff`'s fields from either form.
fn diff_fields<'a>(fields: &[Field<'a>]) -> Option<DiffFields<'a>> {
  match fields {
    [
      Field::Revision(revision),
      Field::String(Some(target)),
      Field::Boolean(recurse),
      Field::Boolean(ancestry),
      Field::String(Some(url)),
    ] => Some((*revision, target, *recurse, *ancestry, url, true, None)),
    [
      Field::Revision(revision),
      Field::String(Some(target)),
      Field::Boolean(recurse),
      Field::Boolean(ancestry),
      Field::String(Some(url)),
      Field::Boolean(text_deltas),
      Field::Word(depth),
    ] => Some((
      *revision,
      target,
      *recurse,
      *ancestry,
      url,
      *text_deltas,
      *depth,
    )),
    _ => None,
  }
}

/// The depth a command asks for: its word's, else infinity when it recurses and `shallow` when not
/// (`SVN_DEPTH_INFINITY_OR_FILES`, `SVN_DEPTH_INFINITY_OR_EMPTY`).
fn requested_depth(word: Option<&str>, recurse: bool, shallow: Depth) -> Depth {
  match word {
    Some(word) => Depth::from_word(word),
    None if recurse => Depth::Infinity,
    None => shallow,
  }
}

/// `bytes` as text: no NUL byte and UTF-8, or the refusal.
fn text(bytes: &[u8]) -> Result<&str, Refused> {
  if bytes.contains(&0) {
    return Err(Refused::Malformed);
  }
  std::str::from_utf8(bytes).map_err(|_| Refused::Malformed)
}

/// A relative path in Subversion's canonical form, as `svn_relpath_canonicalize` makes it: its
/// empty and `.` names dropped, the rest joined by single separators.
fn canonical_relpath(path: &str) -> String {
  let names: Vec<&str> = path
    .split(SEPARATOR)
    .filter(|name| !name.is_empty() && *name != CURRENT_DIRECTORY)
    .collect();
  names.join("/")
}

/// `relpath` joined beneath `base`, as `svn_relpath_join` joins them.
fn join_relpath(base: &str, relpath: &str) -> String {
  match (base.is_empty(), relpath.is_empty()) {
    (true, _) => relpath.to_owned(),
    (false, true) => base.to_owned(),
    (false, false) => format!("{base}/{relpath}"),
  }
}

/// A repository path in Subversion's canonical form, as `svn_fspath__canonicalize` makes it.
fn canonical_fspath(path: &str) -> String {
  format!("/{}", canonical_relpath(path))
}

/// `relpath` joined beneath the repository path `base`, as `svn_fspath__join` joins them.
pub(crate) fn join_fspath(base: &str, relpath: &str) -> String {
  match (base == "/", relpath.is_empty()) {
    (_, true) => base.to_owned(),
    (true, false) => format!("/{relpath}"),
    (false, false) => format!("{base}/{relpath}"),
  }
}

/// `url` decoded as `svn_path_uri_decode` decodes it: each `%` with two hexadecimal digits the byte
/// they name, and `+` a space past a `?`.
fn uri_decode(url: &str) -> Vec<u8> {
  let bytes = url.as_bytes();
  let mut decoded = Vec::with_capacity(bytes.len());
  let mut query = false;
  let mut at = 0usize;
  while let Some(&byte) = bytes.get(at) {
    let escaped = match (
      byte,
      bytes.get(at.saturating_add(1)),
      bytes.get(at.saturating_add(2)),
    ) {
      (ESCAPE, Some(&high), Some(&low)) => hex_pair(high, low),
      _ => None,
    };
    let (value, width) = match escaped {
      Some(value) => (value, 3),
      None if byte == QUERY_SPACE && query => (b' ', 1),
      None => (byte, 1),
    };
    query |= byte == QUERY && escaped.is_none();
    decoded.push(value);
    at = at.saturating_add(width);
  }
  decoded
}

/// The byte two hexadecimal digits name, either case, or none.
fn hex_pair(high: u8, low: u8) -> Option<u8> {
  let high = char::from(high).to_digit(HEX_RADIX)?;
  let low = char::from(low).to_digit(HEX_RADIX)?;
  u8::try_from(high.checked_shl(HEX_BITS)? | low).ok()
}

/// The repository path `url` names beneath the session's repository at `root_url`, as `serve.c`
/// `get_fs_path` finds it, the two decoded: what follows the repository's URL, `/` for nothing; a
/// URL that does not begin with the repository's, followed by a `/` or by nothing, is not the
/// repository's.
pub(crate) fn repository_path(root_url: &[u8], url: &str) -> Result<String, ReportFailure> {
  let root = uri_decode(&String::from_utf8_lossy(root_url));
  let decoded = uri_decode(url);
  let not_ours = || ReportFailure::NotSameRepository {
    url: String::from_utf8_lossy(&decoded).into_owned(),
    repository: String::from_utf8_lossy(&root).into_owned(),
  };
  let rest = decoded.strip_prefix(root.as_slice()).ok_or_else(not_ours)?;
  if !(rest.is_empty() || rest.starts_with(b"/")) {
    return Err(not_ours());
  }
  let rest = text(rest).map_err(|_| not_ours())?;
  Ok(canonical_fspath(rest))
}

/// One path of a report, as `reporter.c`'s `path_info_t` keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathInfo {
  /// The path, relative to the working copy's anchor (joined to the command's target).
  pub path: String,
  /// For `link-path`, the repository path the working copy path is switched to.
  pub link_path: Option<String>,
  /// The revision the path is at; none for `delete-path`.
  pub revision: Option<Revision>,
  /// The path's depth in the working copy.
  pub depth: Depth,
  /// Whether the working copy holds none of the path's entries yet.
  pub start_empty: bool,
  /// The lock token the working copy holds for the path.
  pub lock_token: Option<Vec<u8>>,
}

impl PathInfo {
  /// What the path costs the report's room: its strings and its own size.
  fn cost(&self) -> usize {
    let strings = self
      .path
      .len()
      .saturating_add(self.link_path.as_ref().map_or(0, String::len))
      .saturating_add(self.lock_token.as_ref().map_or(0, Vec::len));
    strings.saturating_add(PATH_INFO_BYTES)
  }
}

/// A command failure the reporter answers with, each in `svnserve`'s words; the command is over and
/// the connection reads its next command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportFailure {
  /// `update` of a target the principal may not read: "Authorization failed" (170001).
  NotAuthorized,
  /// A requested depth of `exclude`: "Request depth 'exclude' not supported" (165002).
  RequestDepthExclude,
  /// A URL outside the session's repository: "'`url`' is not the same repository as '`repository`'"
  /// (170000), each decoded.
  NotSameRepository {
    /// The URL, decoded.
    url: String,
    /// The repository's URL, decoded.
    repository: String,
  },
  /// A reported depth Subversion does not know: "Unsupported report depth 'unknown'" (165002).
  UnsupportedDepth,
  /// A link at depth `exclude`: "Depth 'exclude' not supported for link" (165002).
  ExcludeLink,
  /// A report whose first path is no `set-path` of the target: "Invalid report for top level of
  /// working copy" (165004).
  InvalidTopLevel,
  /// A second report of the target where the target is the anchor: "Two top-level reports with no
  /// target" (165004).
  TwoTopLevelReports,
  /// A command the report does not know: "Unknown editor command '`name`'" (210001); the report
  /// goes on.
  UnknownCommand {
    /// The command's word.
    name: String,
  },
}

impl ReportFailure {
  /// The number of the Subversion error the failure stands for.
  pub fn subversion_code(&self) -> u32 {
    match self {
      Self::NotAuthorized => SVN_ERR_RA_NOT_AUTHORIZED,
      Self::RequestDepthExclude | Self::UnsupportedDepth | Self::ExcludeLink => {
        SVN_ERR_REPOS_BAD_ARGS
      }
      Self::NotSameRepository { .. } => SVN_ERR_RA_ILLEGAL_URL,
      Self::InvalidTopLevel | Self::TwoTopLevelReports => SVN_ERR_REPOS_BAD_REVISION_REPORT,
      Self::UnknownCommand { .. } => SVN_ERR_RA_SVN_UNKNOWN_CMD,
    }
  }

  /// The message `svnserve` writes.
  pub fn message(&self) -> String {
    match self {
      Self::NotAuthorized => "Authorization failed".to_owned(),
      Self::RequestDepthExclude => "Request depth 'exclude' not supported".to_owned(),
      Self::NotSameRepository { url, repository } => {
        format!("'{url}' is not the same repository as '{repository}'")
      }
      Self::UnsupportedDepth => format!("Unsupported report depth '{}'", Depth::Unknown.word()),
      Self::ExcludeLink => "Depth 'exclude' not supported for link".to_owned(),
      Self::InvalidTopLevel => "Invalid report for top level of working copy".to_owned(),
      Self::TwoTopLevelReports => "Two top-level reports with no target".to_owned(),
      Self::UnknownCommand { name } => format!("Unknown editor command '{name}'"),
    }
  }

  /// Appends the failure as a release build of `svnserve` writes a command failure.
  pub(crate) fn write(&self, out: &mut Vec<u8>) -> Result<(), Refused> {
    handshake::write_command_failure(self.subversion_code(), self.message().as_bytes(), out)
      .map_err(|_| Refused::Malformed)
  }
}

/// Why the connection ends: nothing more is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
  /// The client's bytes are no item within the bounds (the codec's refusal).
  Item(ItemError),
  /// A command or its parameters that `vparse_tuple` refuses, or a path, URL or token holding a NUL
  /// byte or no UTF-8: "Malformed network data" (210004).
  Malformed,
  /// A report past the caller's room: "Client request too long" (210009).
  ReportTooLarge,
  /// A call the reporter does not take where it stands: the caller's error, not the client's.
  OutOfTurn,
  /// A drive's call out of the editor's order, or past the nodes open at once: slates' own mistake,
  /// never the client's, refused rather than written.
  Editor(editor::EditorRefused),
}

impl Refused {
  /// The number of the Subversion error the refusal stands for; none for the caller's own error.
  pub fn subversion_code(self) -> Option<u32> {
    match self {
      Self::Item(error) => Some(error.subversion_code()),
      Self::Malformed => Some(SVN_ERR_RA_SVN_MALFORMED_DATA),
      Self::ReportTooLarge => Some(SVN_ERR_RA_SVN_REQUEST_SIZE),
      Self::OutOfTurn | Self::Editor(_) => None,
    }
  }
}

/// A finished report, checked as `finish_report` checks it, which the editor drive answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
  /// The command.
  pub kind: RequestKind,
  /// The repository path of the working copy's anchor (`fs_base`).
  pub anchor: String,
  /// The anchor-relative target (`s_operand`), `""` when the anchor is the target.
  pub operand: String,
  /// The repository path the drive brings the target to (`t_path`).
  pub target_path: String,
  /// The revision the drive brings the target to (`t_rev`).
  pub target_revision: Revision,
  /// The revision the working copy's anchor is at (`s_rev`).
  pub source_revision: Revision,
  /// The depth the command asked for.
  pub requested_depth: Depth,
  /// Whether file texts are sent.
  pub text_deltas: bool,
  /// Whether an added file names what it was copied from.
  pub send_copyfrom_args: bool,
  /// Whether two nodes of one kind are related whatever their history.
  pub ignore_ancestry: bool,
  /// Whether the drive brings the target to another path (`switch`, `diff`).
  pub is_switch: bool,
  /// The report of the target itself.
  pub target_info: PathInfo,
  /// The rest of the report, in the order the client gave it.
  pub paths: Vec<PathInfo>,
}

/// What [`Reporter::begin`] made of a command.
#[derive(Clone, Debug)]
pub enum Begun {
  /// The command was answered and its report is awaited ([`Reporter::receive`]).
  Reporting(Reporter),
  /// The command was refused with this failure, written; the connection reads its next command.
  Failed(ReportFailure),
}

/// What one call of [`Reporter::receive`] made of the client's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
  /// The buffer ends inside the client's next item: at least `needed` bytes from its start are
  /// needed.
  NeedMore {
    /// How many bytes from the start of the buffer the item needs at least.
    needed: usize,
  },
  /// A report command was taken, any failure it is answered with written; the report goes on.
  Took {
    /// How many bytes from the start of the buffer it took.
    consumed: usize,
  },
  /// `finish-report` was read and its trivial auth request written: the report is ready for the
  /// drive.
  Finished {
    /// The report.
    report: Box<Report>,
    /// How many bytes from the start of the buffer it took.
    consumed: usize,
  },
  /// `finish-report` was read and the report refused with this failure, written: the command is
  /// over.
  Failed {
    /// Why.
    failure: ReportFailure,
    /// How many bytes from the start of the buffer it took.
    consumed: usize,
  },
  /// `abort-report` was read and answered: the command is over.
  Aborted {
    /// How many bytes from the start of the buffer it took.
    consumed: usize,
  },
}

/// What a command's report is checked and joined against.
#[derive(Clone, Copy, Debug)]
pub struct Context<'a> {
  /// The established session.
  pub session: &'a Session,
  /// The repository path the session's URL names, `/` for the repository's root.
  pub session_path: &'a str,
  /// The youngest revision, which a command naming none asks for.
  pub youngest: Revision,
  /// For `update`: whether the principal may read the target (`must_have_access`).
  pub readable: bool,
  /// The most bytes the report may hold.
  pub room: usize,
  /// The most bytes one report command's item may take.
  pub item_bytes: usize,
  /// The most nodes a drive holds open at once: the deepest path the repository holds, in names,
  /// and its root (the caller's, from its index).
  pub most_open: usize,
}

/// The reporter of one command (module documentation).
#[derive(Clone, Debug)]
pub struct Reporter {
  kind: RequestKind,
  anchor: String,
  operand: String,
  target_path: String,
  target_revision: Revision,
  requested_depth: Depth,
  text_deltas: bool,
  send_copyfrom_args: bool,
  ignore_ancestry: bool,
  root_url: Vec<u8>,
  paths: Vec<PathInfo>,
  kept: Option<ReportFailure>,
  held: usize,
  room: usize,
  parser: Parser,
  done: bool,
}

impl Reporter {
  /// Answers `request` on `context`'s session as `svnserve`'s command does, appending to `out`: the
  /// report is then awaited, or the command was refused and is over.
  ///
  /// # Errors
  /// None in practice: the answers' words are the grammar's.
  pub fn begin(
    context: &Context<'_>,
    request: Request,
    out: &mut Vec<u8>,
  ) -> Result<Begun, Refused> {
    if request.kind == RequestKind::Update && !context.readable {
      ReportFailure::NotAuthorized.write(out)?;
      return Ok(Begun::Failed(ReportFailure::NotAuthorized));
    }
    write_trivial_auth(out)?;
    let anchor = canonical_fspath(context.session_path);
    let target_path = match (&request.url, request.kind) {
      (Some(url), RequestKind::Switch | RequestKind::Diff) => {
        match repository_path(&context.session.root_url, url) {
          Ok(path) => path,
          Err(failure) => return refused_with(failure, out),
        }
      }
      _ => join_fspath(&anchor, &request.target),
    };
    if request.depth == Depth::Exclude {
      return refused_with(ReportFailure::RequestDepthExclude, out);
    }
    Ok(Begun::Reporting(Reporter {
      kind: request.kind,
      anchor,
      operand: request.target,
      target_path,
      target_revision: request.revision.unwrap_or(context.youngest),
      requested_depth: request.depth,
      text_deltas: request.text_deltas,
      send_copyfrom_args: request.send_copyfrom_args,
      ignore_ancestry: request.ignore_ancestry,
      root_url: context.session.root_url.clone(),
      paths: Vec::new(),
      kept: None,
      held: 0,
      room: context.room,
      parser: Parser::new(report_bounds(context.item_bytes)),
      done: false,
    }))
  }

  /// Reads on in `buffer`, the same buffer with more at its end, as the item codec takes it: the
  /// client's next report command, appending to `out` whatever it is answered with.
  ///
  /// # Errors
  /// Every [`Refused`] but `OutOfTurn` for the client's bytes, which end the connection;
  /// `OutOfTurn` once the report is over.
  pub fn receive(&mut self, buffer: &[u8], out: &mut Vec<u8>) -> Result<Progress, Refused> {
    if self.done {
      return Err(Refused::OutOfTurn);
    }
    let (value, consumed) = match self.parser.parse(buffer) {
      Ok(Parsed::NeedMore { needed }) => return Ok(Progress::NeedMore { needed }),
      Ok(Parsed::Item { value, consumed }) => (value, consumed),
      Err(error) => {
        self.done = true;
        return Err(Refused::Item(error));
      }
    };
    let progress = self.command(&value, consumed, out);
    if !matches!(progress, Ok(Progress::Took { .. })) {
      self.done = true;
    }
    progress
  }

  /// One report command, `( word ( params ) )`.
  fn command(
    &mut self,
    value: &Value<'_>,
    consumed: usize,
    out: &mut Vec<u8>,
  ) -> Result<Progress, Refused> {
    let Value::List(items) = value else {
      return Err(Refused::Malformed);
    };
    let fields = tuple::read(items, COMMAND_FORMAT).map_err(|_| Refused::Malformed)?;
    let [Field::Word(Some(name)), Field::List(Some(params))] = fields.as_slice() else {
      return Err(Refused::Malformed);
    };
    match *name {
      SET_PATH => self.set_path(params)?,
      DELETE_PATH => self.delete_path(params)?,
      LINK_PATH => self.link_path(params)?,
      FINISH_REPORT => return self.finish(consumed, out),
      ABORT_REPORT => {
        write_empty_success(out)?;
        return Ok(Progress::Aborted { consumed });
      }
      other => ReportFailure::UnknownCommand {
        name: other.to_owned(),
      }
      .write(out)?,
    }
    Ok(Progress::Took { consumed })
  }

  /// `set-path`: `crb?(?c)?w`, depth infinity when no word is sent.
  fn set_path(&mut self, params: &[Value<'_>]) -> Result<(), Refused> {
    let fields = tuple::read(params, SET_PATH_FORMAT).map_err(|_| Refused::Malformed)?;
    let [Field::String(Some(path)), rest @ ..] = fields.as_slice() else {
      return Err(Refused::Malformed);
    };
    let reported = Reported::of(rest)?;
    if reported.depth == Depth::Unknown {
      return self.keep(ReportFailure::UnsupportedDepth);
    }
    let info = reported.at(
      join_relpath(&self.operand, &canonical_relpath(text(path)?)),
      None,
    );
    self.record(info)
  }

  /// `delete-path`: `c`, a deletion at depth infinity.
  fn delete_path(&mut self, params: &[Value<'_>]) -> Result<(), Refused> {
    let fields = tuple::read(params, DELETE_PATH_FORMAT).map_err(|_| Refused::Malformed)?;
    let [Field::String(Some(path))] = fields.as_slice() else {
      return Err(Refused::Malformed);
    };
    self.record(PathInfo {
      path: join_relpath(&self.operand, &canonical_relpath(text(path)?)),
      link_path: None,
      revision: None,
      depth: Depth::Infinity,
      start_empty: false,
      lock_token: None,
    })
  }

  /// `link-path`: `ccrb?(?c)?w`, its URL turned into a repository path as `get_fs_path` turns it; a
  /// URL elsewhere, then a depth of `exclude`, then one Subversion does not know is the refusal
  /// kept.
  fn link_path(&mut self, params: &[Value<'_>]) -> Result<(), Refused> {
    let fields = tuple::read(params, LINK_PATH_FORMAT).map_err(|_| Refused::Malformed)?;
    let [
      Field::String(Some(path)),
      Field::String(Some(url)),
      rest @ ..,
    ] = fields.as_slice()
    else {
      return Err(Refused::Malformed);
    };
    let path = join_relpath(&self.operand, &canonical_relpath(text(path)?));
    let url = text(url)?;
    let reported = Reported::of(rest)?;
    let refusal = match repository_path(&self.root_url, url) {
      Err(failure) => Some(failure),
      Ok(_) if reported.depth == Depth::Exclude => Some(ReportFailure::ExcludeLink),
      Ok(_) if reported.depth == Depth::Unknown => Some(ReportFailure::UnsupportedDepth),
      Ok(link_path) => return self.record(reported.at(path, Some(link_path))),
    };
    refusal.map_or(Ok(()), |failure| self.keep(failure))
  }

  /// Keeps the report's first refusal; a report already refused keeps the first.
  fn keep(&mut self, failure: ReportFailure) -> Result<(), Refused> {
    if self.kept.is_none() {
      self.kept = Some(failure);
    }
    Ok(())
  }

  /// Records a path of the report, unless the report was refused already, within the room.
  fn record(&mut self, info: PathInfo) -> Result<(), Refused> {
    if self.kept.is_some() {
      return Ok(());
    }
    self.held = self.held.saturating_add(info.cost());
    if self.held > self.room {
      return Err(Refused::ReportTooLarge);
    }
    self.paths.push(info);
    Ok(())
  }

  /// `finish-report`: a trivial auth request, then the kept refusal or the report checked.
  fn finish(&mut self, consumed: usize, out: &mut Vec<u8>) -> Result<Progress, Refused> {
    write_trivial_auth(out)?;
    let checked = match self.kept.take() {
      Some(failure) => Err(failure),
      None => self.top_level(),
    };
    match checked {
      Ok(report) => Ok(Progress::Finished {
        report: Box::new(report),
        consumed,
      }),
      Err(failure) => {
        failure.write(out)?;
        Ok(Progress::Failed { failure, consumed })
      }
    }
  }

  /// The report's top level checked as `finish_report` checks it: the first path a `set-path` of
  /// the target, then a second report of the target, allowed only below the anchor, taken as the
  /// target's.
  fn top_level(&mut self) -> Result<Report, ReportFailure> {
    let mut paths = std::mem::take(&mut self.paths).into_iter().peekable();
    let first = paths.next().ok_or(ReportFailure::InvalidTopLevel)?;
    let source_revision = match (&first.link_path, first.revision) {
      (None, Some(revision)) if first.path == self.operand => revision,
      _ => return Err(ReportFailure::InvalidTopLevel),
    };
    let target_info = match paths.next_if(|next| next.path == self.operand) {
      Some(_) if self.operand.is_empty() => return Err(ReportFailure::TwoTopLevelReports),
      Some(mut second) => {
        if second.revision.is_none() {
          second.depth = first.depth;
        }
        second
      }
      None => first,
    };
    Ok(self.report(source_revision, target_info, paths.collect()))
  }

  /// The finished report.
  fn report(
    &self,
    source_revision: Revision,
    target_info: PathInfo,
    paths: Vec<PathInfo>,
  ) -> Report {
    Report {
      kind: self.kind,
      anchor: self.anchor.clone(),
      operand: self.operand.clone(),
      target_path: self.target_path.clone(),
      target_revision: self.target_revision,
      source_revision,
      requested_depth: self.requested_depth,
      text_deltas: self.text_deltas,
      send_copyfrom_args: self.send_copyfrom_args,
      ignore_ancestry: self.ignore_ancestry,
      is_switch: matches!(self.kind, RequestKind::Switch | RequestKind::Diff),
      target_info,
      paths,
    }
  }
}

/// The bounds a report command is read within: the caller's bytes, the longest word, the deepest
/// list a report command holds, and the count the bytes pay for parsed.
fn report_bounds(item_bytes: usize) -> Bounds {
  Bounds {
    item_bytes,
    word_bytes: LONGEST_WORD,
    depth: REPORT_DEPTH,
    nodes: item_bytes.checked_div(PARSED_ITEM_BYTES).unwrap_or(0),
  }
}

/// What `set-path` and `link-path` report after their path (and URL): `rb?(?c)?w`.
struct Reported {
  revision: Option<Revision>,
  start_empty: bool,
  lock_token: Option<Vec<u8>>,
  depth: Depth,
}

impl Reported {
  /// The fields after the path, a depth word's absence depth infinity; a token holding a NUL byte
  /// is refused.
  fn of(fields: &[Field<'_>]) -> Result<Self, Refused> {
    let [
      Field::Revision(revision),
      Field::Boolean(start_empty),
      Field::String(token),
      Field::Word(depth),
    ] = fields
    else {
      return Err(Refused::Malformed);
    };
    if token.is_some_and(|token| token.contains(&0)) {
      return Err(Refused::Malformed);
    }
    Ok(Self {
      revision: *revision,
      start_empty: *start_empty,
      lock_token: token.map(<[u8]>::to_vec),
      depth: depth.map_or(Depth::Infinity, Depth::from_word),
    })
  }

  /// The path reported at `path`, switched to `link_path` where linked.
  fn at(self, path: String, link_path: Option<String>) -> PathInfo {
    PathInfo {
      path,
      link_path,
      revision: self.revision,
      depth: self.depth,
      start_empty: self.start_empty,
      lock_token: self.lock_token,
    }
  }
}

/// Appends `failure`, written, as the command's answer: the command is over.
fn refused_with(failure: ReportFailure, out: &mut Vec<u8>) -> Result<Begun, Refused> {
  failure.write(out)?;
  Ok(Begun::Failed(failure))
}

/// Appends a trivial auth request, `( success ( ( ) 0: ) )`: no authentication needed
/// (`serve.c` `trivial_auth_request`).
pub(crate) fn write_trivial_auth(out: &mut Vec<u8>) -> Result<(), Refused> {
  let params = Value::List(vec![Value::List(Vec::new()), Value::String(&[])]);
  ra_svn::write(&Value::List(vec![Value::Word(SUCCESS), params]), out)
    .map_err(|_| Refused::Malformed)
}

/// Appends an empty command response, `( success ( ) )`.
pub(crate) fn write_empty_success(out: &mut Vec<u8>) -> Result<(), Refused> {
  let params = Value::List(Vec::new());
  ra_svn::write(&Value::List(vec![Value::Word(SUCCESS), params]), out)
    .map_err(|_| Refused::Malformed)
}

#[cfg(test)]
mod tests;
