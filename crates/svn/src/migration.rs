//! A repository's history in and out as a dump stream, the server's migration verbs (§4.17.4 "Pieces that follow"
//! piece 6, as Ada scoped it on 2026-11-19; A-628): an existing Subversion repository's `svnadmin dump` imported as the
//! history of a repository slates created, and a repository's history exported as a dump stream any stock `svnadmin
//! load` takes. Stock tools stay stock; slates offers the server side alone, so there is no command line here and no
//! tool's text to match, only the dump stream itself and typed refusals.
//!
//! **The records** ([`Repository`]): what a Subversion repository slates serves is made of, as these verbs read and
//! write it: the Subversion index (A-553), every node-revision computed from each revision's changed-path rows; each
//! revision's properties; every text and property list held once by its content identity (BLAKE3, the identity the
//! green and the shared object store name content by, §4.11, A-581); and the repository's UUID. A repository
//! `CreateRepository` made (A-575) starts as [`Repository::new`] leaves one: revision 0, an empty root, `svn:date`.
//!
//! **Import** ([`Importer`]) reads a dump through A-593's reader as its bytes arrive and makes each revision as FSFS
//! makes it through `load-fs-vtable.c`: a transaction on the youngest revision, each record's action applied to it
//! (a copy from the revision the dump names, a replacement a deletion and an addition), the properties set whole or
//! changed, the text set whole or a delta applied to its base, every checksum the stream carries checked (the text's,
//! a delta's base's, a copy source's), then the transaction's changed paths appended to the index as one revision with
//! the stream's revision properties. The history keeps its revision numbers: a stream continues the repository's
//! history (revision 0 into an empty repository, else the revision after the youngest), so a dump made in parts loads
//! part after part; the UUID record names the repository, set into one that holds no revision and refused when it
//! names another. Properties are kept byte for byte, as the repository held them.
//!
//! **Export** ([`Exporter`]) writes a range of revisions as `svn_repos_dump_fs4` writes the same range: a version and
//! a UUID record, each revision's record with its properties, then the revision replayed (`svn_repos_replay2`): its
//! changed paths in `svn_path_compare_paths` order through the path driver, each record's sections decided as
//! `dump_node` decides them (a node's properties and text written when they differ from what it is compared with: its
//! own previous revision, or its copy source, or the copy source of a directory copied above it), a directory's
//! deletions written when it closes, a replacement by a copy a deletion record and an addition. A range that starts
//! after revision 0 is written incrementally, every revision replayed, as a migration in parts needs; the whole tree
//! `svnadmin dump` writes for a range that is not incremental is not offered. Format 3 (`--deltas`) is owed with
//! Subversion's own deltifier (part 2), since only its windows make `svnadmin`'s bytes.
//!
//! **The gate**: import(export(R)) is R, and export(import(D)) is D byte for byte for the golden dumps stock
//! `svnadmin` 1.15.0 wrote (A-593's `fixtures/dump`).
//!
//! The evidence: `docs/wip/research/vcs-servers-subversion.md` §14. No I/O, no clock, no randomness, no panic.

mod export;
mod import;
mod paths;
mod properties;
mod records;

#[cfg(test)]
mod tests;

pub use export::{Exporter, Exporting};
pub use import::{Importer, Importing};
pub use records::{Properties, Repository, Room};

use crate::dump::{Digest, DumpError, Uuid};
use crate::index::{IndexRefusal, Revision};
use crate::svndiff::SvndiffError;

/// Why an import or an export was refused. Each carries the number of the Subversion error the same refusal is in
/// Subversion's own load or dump ([`MigrationRefusal::subversion_code`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationRefusal {
  /// The dump stream was refused by the dump codec (A-593), with its own number.
  Stream(DumpError),
  /// A text delta that does not apply to its base (svndiff's own number).
  Delta(SvndiffError),
  /// A revision record that does not continue the repository's history: revision 0 into a repository that holds
  /// revisions, or a revision other than the one after the youngest (`SVN_ERR_FS_NO_SUCH_REVISION`).
  OutOfSequence {
    /// The revision the history continues with.
    expected: Revision,
    /// The revision the stream named.
    found: Revision,
  },
  /// A UUID record naming another repository than the one whose history continues (`SVN_ERR_BAD_UUID`).
  ForeignRepository {
    /// The repository's UUID.
    repository: Uuid,
    /// The stream's.
    stream: Uuid,
  },
  /// A path that is not UTF-8 or not in Subversion's canonical form (`SVN_ERR_FS_PATH_SYNTAX`).
  PathSyntax {
    /// The path, as the stream gave it.
    path: Vec<u8>,
  },
  /// A record that adds, deletes or replaces the root (`SVN_ERR_FS_ROOT_DIR`).
  RootChange,
  /// A path absent where it must stand: in the transaction, or a copy source in its revision
  /// (`SVN_ERR_FS_NOT_FOUND`).
  NotFound {
    /// The revision looked in; none for the transaction being made.
    revision: Option<Revision>,
    /// The path.
    path: String,
  },
  /// An addition where a node stands (`SVN_ERR_FS_ALREADY_EXISTS`).
  AlreadyExists {
    /// The path.
    path: String,
  },
  /// A directory of a path that is a file (`SVN_ERR_FS_NOT_DIRECTORY`).
  NotDirectory {
    /// The file.
    path: String,
  },
  /// A record whose kind is not the kind of the node it changes or copies (`SVN_ERR_NODE_UNEXPECTED_KIND`).
  KindMismatch {
    /// The path.
    path: String,
  },
  /// A copy from the revision being made or later (`SVN_ERR_FS_NO_SUCH_REVISION`).
  CopyFromFuture {
    /// The copy's source revision.
    revision: Revision,
  },
  /// A text whose digest is not the one its header names (`SVN_ERR_CHECKSUM_MISMATCH`).
  ChecksumMismatch {
    /// The node's path.
    path: String,
    /// The digest the header names.
    expected: Digest,
    /// The text's.
    actual: Digest,
  },
  /// A delta whose base's digest is not the one its header names (`SVN_ERR_CHECKSUM_MISMATCH`).
  BaseChecksumMismatch {
    /// The node's path.
    path: String,
    /// The digest the header names.
    expected: Digest,
    /// The base's.
    actual: Digest,
  },
  /// A copy whose source's text is not the one its header names (`SVN_ERR_CHECKSUM_MISMATCH`).
  CopySourceChecksumMismatch {
    /// The copy's path.
    path: String,
    /// The digest the header names.
    expected: Digest,
    /// The source's.
    actual: Digest,
  },
  /// The revision an import made, refused by the Subversion index (A-553) with its own number.
  Index(IndexRefusal),
  /// Records that would pass the room their caller gives (`SVN_ERR_FS_GENERAL`: Subversion has no closer error).
  Room {
    /// The bytes they would hold.
    needed: u64,
    /// The room's bytes.
    room: u64,
  },
  /// A text or a property list the records name and do not hold, or a count past 2^64 - 1 (`SVN_ERR_FS_CORRUPT`).
  Corrupt,
  /// An export range whose first revision is past its last, or whose last is past the youngest
  /// (`SVN_ERR_REPOS_BAD_ARGS`).
  BadRange {
    /// The range's first revision.
    first: Revision,
    /// Its last.
    last: Revision,
    /// The youngest revision.
    youngest: Revision,
  },
  /// An export in format 3, owed with Subversion's deltifier (`SVN_ERR_UNSUPPORTED_FEATURE`).
  DeltasOwed,
}

/// `SVN_ERR_BAD_UUID`.
/// Format: the bad category's start, 125,000, plus 8 (`svn_error_codes.h`).
const SVN_ERR_BAD_UUID: u32 = 125_008;
/// `SVN_ERR_FS_GENERAL`.
/// Format: the filesystem category's start, 160,000, plus 0 (`svn_error_codes.h`).
const SVN_ERR_FS_GENERAL: u32 = 160_000;
/// `SVN_ERR_NODE_UNEXPECTED_KIND`.
/// Format: the node category's start, 145,000, plus 1 (`svn_error_codes.h`).
const SVN_ERR_NODE_UNEXPECTED_KIND: u32 = 145_001;
/// `SVN_ERR_FS_CORRUPT`.
/// Format: the filesystem category's start, 160,000, plus 4 (`svn_error_codes.h`).
const SVN_ERR_FS_CORRUPT: u32 = 160_004;
/// `SVN_ERR_FS_PATH_SYNTAX`.
/// Format: the filesystem category's start plus 5 (`svn_error_codes.h`).
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
/// `SVN_ERR_FS_ROOT_DIR`.
/// Format: the filesystem category's start plus 21 (`svn_error_codes.h`).
const SVN_ERR_FS_ROOT_DIR: u32 = 160_021;
/// `SVN_ERR_REPOS_BAD_ARGS`.
/// Format: the repository category's start, 165,000, plus 4 (`svn_error_codes.h`).
const SVN_ERR_REPOS_BAD_ARGS: u32 = 165_004;
/// `SVN_ERR_CHECKSUM_MISMATCH`.
/// Format: the miscellaneous category's start, 200,000, plus 14 (`svn_error_codes.h`).
const SVN_ERR_CHECKSUM_MISMATCH: u32 = 200_014;
/// `SVN_ERR_UNSUPPORTED_FEATURE`.
/// Format: the miscellaneous category's start plus 7 (`svn_error_codes.h`).
const SVN_ERR_UNSUPPORTED_FEATURE: u32 = 200_007;

impl MigrationRefusal {
  /// The number of the Subversion error the same refusal is in Subversion's own load or dump.
  pub fn subversion_code(&self) -> u32 {
    match self {
      Self::Stream(refusal) => refusal.subversion_code(),
      Self::Delta(refusal) => refusal.subversion_code(),
      Self::Index(refusal) => refusal.subversion_code(),
      Self::OutOfSequence { .. } | Self::CopyFromFuture { .. } => SVN_ERR_FS_NO_SUCH_REVISION,
      Self::ForeignRepository { .. } => SVN_ERR_BAD_UUID,
      Self::PathSyntax { .. } => SVN_ERR_FS_PATH_SYNTAX,
      Self::RootChange => SVN_ERR_FS_ROOT_DIR,
      Self::NotFound { .. } => SVN_ERR_FS_NOT_FOUND,
      Self::AlreadyExists { .. } => SVN_ERR_FS_ALREADY_EXISTS,
      Self::NotDirectory { .. } => SVN_ERR_FS_NOT_DIRECTORY,
      Self::KindMismatch { .. } => SVN_ERR_NODE_UNEXPECTED_KIND,
      Self::ChecksumMismatch { .. }
      | Self::BaseChecksumMismatch { .. }
      | Self::CopySourceChecksumMismatch { .. } => SVN_ERR_CHECKSUM_MISMATCH,
      Self::Room { .. } => SVN_ERR_FS_GENERAL,
      Self::Corrupt => SVN_ERR_FS_CORRUPT,
      Self::BadRange { .. } => SVN_ERR_REPOS_BAD_ARGS,
      Self::DeltasOwed => SVN_ERR_UNSUPPORTED_FEATURE,
    }
  }
}
