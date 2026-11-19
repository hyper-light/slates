//! svnadmin's verbs over a repository's records (§4.17.4 "Pieces that follow" piece 6, its rest; A-628), each a pure
//! verb whose output and errors are stock `svnadmin` 1.15's. Part 1: `load` and `dump` in format 2, whose round trip is
//! piece 6's gate: a dump stock `svnadmin` wrote, loaded and dumped again, is the same bytes.
//!
//! The repository ([`Repository`]) is the records a verb reads and writes: the Subversion index (A-553), each revision's
//! tree, its revision properties, and every text and property list held once by its content identity (BLAKE3, §4.11).
//! The server keeps the same records on the repository's owner (§4.17.1); this is the reference the verbs are written
//! against and proven on.
//!
//! - **Load** ([`Loader`]) reads a dump through A-593's reader as its bytes arrive, applies each revision as FSFS
//!   applies it through `load-fs-vtable.c` (a transaction on the youngest revision; copies from the revisions the dump
//!   named, mapped to the ones they became; `svn:mergeinfo` renumbered and moved; revision 0's properties only into an
//!   empty repository; the UUID record by the option given), commits it to the index, and writes `svnadmin load`'s
//!   progress lines byte for byte (`svnadmin.c` `repos_notify_handler`), including 1.15's "editing path" for every node,
//!   since `load-fs-vtable.c` never names the node's action in its notification.
//! - **Dump** ([`Dumper`]) writes a range of revisions as `svn_repos_dump_fs4` does: revision 0 a bare revision record;
//!   the first revision of a range that is not incremental a whole tree, its directories' entries in the order FSFS's
//!   entries table yields them ([`order`]); every other revision replayed, its changed paths in path order, a
//!   directory's deletions written when it closes; each record's sections decided as `dump_node` decides them, a node's
//!   properties and text written when they differ from what it is compared with. Progress goes to its own stream,
//!   `* Dumped revision N.` a revision.
//! - **Errors** ([`AdminError`]) are typed, and each renders the lines `svnadmin` prints for it, Subversion's numbers
//!   and words, an argument error after "Try 'svnadmin help' for more info" as `svnadmin`'s main wraps it.
//!
//! The evidence: `docs/wip/research/vcs-servers-subversion.md` §13 (the sources at `1.14.x` `d434dc14`) and the golden
//! vectors in `fixtures/svnadmin`, which stock `svnadmin` 1.15.0 wrote (its `make-fixtures.sh`).
//!
//! No I/O, no clock, no randomness, no panic: a UUID or a date a verb needs is given to it.

mod dump;
mod load;
mod order;
mod properties;
mod repository;

#[cfg(test)]
mod tests;

pub use dump::{DumpOptions, Dumper};
pub use load::{LoadOptions, Loader, Loading, UuidAction};
pub use repository::{Entry, Properties, Repository, Tree};

use crate::dump::{Digest, DumpError, Uuid};
use crate::index::{IndexRefusal, Revision};
use crate::mergeinfo::MergeinfoRefusal;

/// `SVN_ERR_CL_ARG_PARSING_ERROR`, the number `svnadmin` answers a bad argument with.
/// Format: the command-line category's start, 205,000, plus 0 (`svn_error_codes.h`).
const SVN_ERR_CL_ARG_PARSING_ERROR: u32 = 205_000;
/// `SVN_ERR_FS_PATH_SYNTAX`.
/// Format: the filesystem category's start, 160,000, plus 5 (`svn_error_codes.h`).
const SVN_ERR_FS_PATH_SYNTAX: u32 = 160_005;
/// `SVN_ERR_FS_NO_SUCH_REVISION`.
/// Format: the filesystem category's start plus 6 (`svn_error_codes.h`).
const SVN_ERR_FS_NO_SUCH_REVISION: u32 = 160_006;
/// `SVN_ERR_FS_NOT_FOUND`.
/// Format: the filesystem category's start plus 13 (`svn_error_codes.h`).
const SVN_ERR_FS_NOT_FOUND: u32 = 160_013;
/// `SVN_ERR_FS_NOT_DIRECTORY`.
/// Format: the filesystem category's start plus 16 (`svn_error_codes.h`).
const SVN_ERR_FS_NOT_DIRECTORY: u32 = 160_016;
/// `SVN_ERR_FS_ALREADY_EXISTS`.
/// Format: the filesystem category's start plus 20 (`svn_error_codes.h`).
const SVN_ERR_FS_ALREADY_EXISTS: u32 = 160_020;
/// `SVN_ERR_BAD_PROPERTY_VALUE`.
/// Format: the bad category's start, 125,000, plus 5 (`svn_error_codes.h`).
const SVN_ERR_BAD_PROPERTY_VALUE: u32 = 125_005;
/// `SVN_ERR_CHECKSUM_MISMATCH`.
/// Format: the miscellaneous category's start, 200,000, plus 14 (`svn_error_codes.h`).
const SVN_ERR_CHECKSUM_MISMATCH: u32 = 200_014;
/// `SVN_ERR_INCOMPLETE_DATA`.
/// Format: the miscellaneous category's start plus 3 (`svn_error_codes.h`).
const SVN_ERR_INCOMPLETE_DATA: u32 = 200_003;

/// What `svnadmin`'s main puts before an argument error (`svnadmin.c`, `main`: `svn_error_quick_wrap`).
const TRY_HELP: &str = "Try 'svnadmin help' for more info";

/// Where a path was looked for: a transaction, by its name, or a revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
  /// The transaction a load builds a revision in.
  Transaction(String),
  /// A committed revision.
  Revision(Revision),
}

/// Why a verb was refused. Each renders the lines `svnadmin` prints for it ([`AdminError::lines`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdminError {
  /// A revision argument past the youngest revision (`svnadmin.c` `get_revnum`).
  PastYoungest {
    /// The youngest revision.
    youngest: Revision,
  },
  /// A range whose first revision is past its second (`get_dump_range`, `get_load_range`).
  RangeBackward,
  /// The dump stream was refused by the dump codec (A-593).
  Stream(DumpError),
  /// A text whose digest is not the one its header names (`svn_fs_apply_text`'s check).
  ChecksumMismatch {
    /// The node's path.
    path: String,
    /// The digest its header names.
    expected: Digest,
    /// The text's.
    actual: Digest,
  },
  /// A delta whose base is not the one its header names (`svn_fs_apply_textdelta`'s check).
  BaseChecksumMismatch {
    /// The node's path.
    path: String,
    /// The digest its header names.
    expected: Digest,
    /// The base's.
    actual: Digest,
  },
  /// A copy whose source's text is not the one `Text-copy-source-md5` names (`maybe_add_with_history`).
  CopySourceChecksumMismatch {
    /// The source's path.
    source: String,
    /// The source's revision in the repository.
    revision: Revision,
    /// The copy's path.
    path: String,
    /// The revision the dump had for the transaction.
    original: Revision,
    /// The digest its header names.
    expected: Digest,
    /// The source's.
    actual: Digest,
  },
  /// An addition where a node stands (`SVN_FS__ALREADY_EXISTS`).
  AlreadyExists {
    /// The repository's UUID, which names its filesystem.
    filesystem: Uuid,
    /// The transaction.
    transaction: String,
    /// The path, as the filesystem writes it.
    path: String,
  },
  /// A path absent where it must stand (`SVN_FS__NOT_FOUND`).
  NotFound {
    /// Where it was looked for.
    place: Place,
    /// The path, as the filesystem writes it.
    path: String,
  },
  /// A directory of a path that is a file (`SVN_FS__ERR_NOT_DIRECTORY`).
  NotDirectory {
    /// The file.
    path: String,
    /// The repository's UUID.
    filesystem: Uuid,
  },
  /// A copy whose source revision the repository does not hold (`maybe_add_with_history`).
  NoSuchCopySource {
    /// The revision the copy was mapped to.
    revision: i64,
  },
  /// A path that is not UTF-8 (`svn_fs__path_valid`).
  PathSyntax {
    /// The path, its bytes as they came.
    path: Vec<u8>,
  },
  /// The revision a load built, refused by the Subversion index (A-553) with its own number.
  Index(IndexRefusal),
  /// A property value the repository layer refuses (`svn_repos__validate_prop`), wrapped by `svnadmin load`'s advice.
  BadProperty {
    /// What the repository layer says.
    reason: String,
  },
  /// `svn:mergeinfo` that does not parse, wrapped as `load-fs-vtable.c` wraps it ("Invalid svn:mergeinfo value").
  Mergeinfo(MergeinfoRefusal),
}

impl AdminError {
  /// The errors `svnadmin` prints for this refusal, outermost first, each a Subversion number and its message; each
  /// line is written `svnadmin: E<number>: <message>` and a line feed ([`AdminError::stderr`]).
  pub fn lines(&self) -> Vec<(u32, String)> {
    match self {
      Self::PastYoungest { youngest } => argument(format!(
        "Revisions must not be greater than the youngest revision ({youngest})"
      )),
      Self::RangeBackward => argument("First revision cannot be higher than second".to_owned()),
      Self::Stream(refusal) => vec![(refusal.subversion_code(), stream_message(*refusal))],
      Self::Index(refusal) => vec![(refusal.subversion_code(), format!("{refusal:?}"))],
      Self::BadProperty { reason } => vec![
        (SVN_ERR_BAD_PROPERTY_VALUE, PROPERTY_ADVICE.to_owned()),
        (SVN_ERR_BAD_PROPERTY_VALUE, reason.clone()),
      ],
      Self::Mergeinfo(refusal) => vec![
        (refusal.subversion_code(), "Invalid svn:mergeinfo value".to_owned()),
        (refusal.subversion_code(), format!("{refusal:?}")),
      ],
      other => vec![other.filesystem_line()],
    }
  }

  /// The one line of a filesystem refusal.
  fn filesystem_line(&self) -> (u32, String) {
    match self {
      Self::ChecksumMismatch {
        path,
        expected,
        actual,
      } => (
        SVN_ERR_CHECKSUM_MISMATCH,
        mismatch(&format!("Checksum mismatch for '{path}'"), expected, actual),
      ),
      Self::BaseChecksumMismatch {
        path,
        expected,
        actual,
      } => (
        SVN_ERR_CHECKSUM_MISMATCH,
        mismatch(&format!("Base checksum mismatch on '{path}'"), expected, actual),
      ),
      Self::CopySourceChecksumMismatch { .. } => self.copy_source_line(),
      other => other.path_line(),
    }
  }

  /// The line of a copy source's checksum mismatch.
  fn copy_source_line(&self) -> (u32, String) {
    let Self::CopySourceChecksumMismatch {
      source,
      revision,
      path,
      original,
      expected,
      actual,
    } = self
    else {
      return (SVN_ERR_CHECKSUM_MISMATCH, String::new());
    };
    let title =
      format!("Copy source checksum mismatch on copy from '{source}'@{revision}\nto '{path}' in rev based on r{original}");
    (SVN_ERR_CHECKSUM_MISMATCH, mismatch(&title, expected, actual))
  }

  /// The line of a refusal about a path.
  fn path_line(&self) -> (u32, String) {
    match self {
      Self::AlreadyExists {
        filesystem,
        transaction,
        path,
      } => (
        SVN_ERR_FS_ALREADY_EXISTS,
        format!(
          "File already exists: filesystem '{}', transaction '{transaction}', path '{path}'",
          String::from_utf8_lossy(filesystem.as_bytes())
        ),
      ),
      Self::NotFound { place, path } => (SVN_ERR_FS_NOT_FOUND, not_found(place, path)),
      Self::NotDirectory { path, filesystem } => (
        SVN_ERR_FS_NOT_DIRECTORY,
        format!(
          "'{path}' is not a directory in filesystem '{}'",
          String::from_utf8_lossy(filesystem.as_bytes())
        ),
      ),
      Self::NoSuchCopySource { revision } => (
        SVN_ERR_FS_NO_SUCH_REVISION,
        format!("Relative source revision {revision} is not available in current repository"),
      ),
      Self::PathSyntax { path } => (
        SVN_ERR_FS_PATH_SYNTAX,
        format!("Path '{}' is not in UTF-8", String::from_utf8_lossy(path)),
      ),
      _ => (SVN_ERR_INCOMPLETE_DATA, String::new()),
    }
  }

  /// The bytes `svnadmin` writes to its error stream for this refusal (`svn_handle_error2`): each line
  /// `svnadmin: E<number>: <message>` and a line feed, the number six digits wide.
  pub fn stderr(&self) -> Vec<u8> {
    self
      .lines()
      .into_iter()
      .flat_map(|(code, message)| format!("svnadmin: E{code:06}: {message}\n").into_bytes())
      .collect()
  }
}

/// What `subcommand_load` adds before a property value the repository layer refused (`svnadmin.c`).
const PROPERTY_ADVICE: &str =
  "Invalid property value found in dumpstream; consider repairing the source or using --bypass-prop-validation while \
   loading.";

/// An argument error's lines: "Try 'svnadmin help' for more info", then the error.
fn argument(message: String) -> Vec<(u32, String)> {
  vec![
    (SVN_ERR_CL_ARG_PARSING_ERROR, TRY_HELP.to_owned()),
    (SVN_ERR_CL_ARG_PARSING_ERROR, message),
  ]
}

/// A checksum mismatch's message as `svn_checksum_mismatch_err` writes it: the title, then the two digests in hex.
fn mismatch(title: &str, expected: &Digest, actual: &Digest) -> String {
  format!(
    "{title}:\n   expected:  {}\n     actual:  {}\n",
    digest_hex(expected),
    digest_hex(actual)
  )
}

/// A digest in lowercase hex.
fn digest_hex(digest: &Digest) -> String {
  let bytes: &[u8] = match digest {
    Digest::Md5(md5) => md5,
    Digest::Sha1(sha1) => sha1,
  };
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A path absent where it must stand, as `SVN_FS__NOT_FOUND` writes it for a transaction or a revision.
fn not_found(place: &Place, path: &str) -> String {
  match place {
    Place::Transaction(name) => format!("File not found: transaction '{name}', path '{path}'"),
    Place::Revision(revision) => format!("File not found: revision {revision}, path '{path}'"),
  }
}

/// The message `load.c` gives a refusal of the dump stream.
fn stream_message(refusal: DumpError) -> String {
  match refusal {
    DumpError::Truncated => "Premature end of content data in dumpstream".to_owned(),
    DumpError::MalformedVersion => "Malformed dumpfile header".to_owned(),
    DumpError::UnsupportedVersion { version } => format!("Unsupported dumpfile version: {version}"),
    DumpError::UnrecognizedRecord => "Unrecognized record type in stream".to_owned(),
    DumpError::ChecksumMismatch { .. } => "A checksum mismatch occurred".to_owned(),
    other => format!("Dumpstream data appears to be malformed ({other:?})"),
  }
}
