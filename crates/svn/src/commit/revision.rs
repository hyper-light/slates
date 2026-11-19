//! One revision as the owner commits it (§4.17.4 item 3.5; A-655 part 2): what the version records, made from a
//! client's accepted work or from one revision of an imported dump, and committed by the one commit step,
//! [`crate::migration::Repository::check`] then [`crate::migration::Repository::apply`], for client commits and
//! imports alike.
//!
//! **The value** ([`Revision`]): the youngest revision it was judged against or made on; its record, the
//! transaction's name and the changed-path rows from which every holder computes the index (item 2); its revision
//! properties, `svn:date` among them; the texts and property lists its rows name, by content identity (a text's
//! bytes, a property list in FSFS's form); and the lock entries its verdict decided. It holds every fact a replay
//! needs and reads no clock, so committing it again reaches the owner's records: a commit's "time and author taken
//! from the merge record, never from a holder's clock" (§4.17.1 item 3).
//!
//! **From a client's work** ([`Revision::of_work`]): the revision properties `svnserve`'s commit leaves, the
//! client's or its log message alone with `svn:author` the principal (A-655 part 1), its ephemeral `svn:txn-`
//! properties dropped after pre-commit as `svn_repos_fs_commit_txn` drops them (`fs-wrap.c` 72-89, the drive's);
//! and `svn:date` the owner's instant at the step as `svn_time_to_cstring` writes it, replacing any the client sent,
//! as FSFS's commit dates a transaction begun without `SVN_FS_TXN_CLIENT_DATE` (`transaction.c`
//! `write_final_revprop` 3617-3630), which `svnserve`'s are (`svn_repos_fs_begin_txn_for_commit2` begins them with
//! `SVN_FS_TXN_CHECK_LOCKS` alone, `fs-wrap.c` 151-152).
//!
//! **From an imported dump revision** (the import's, A-628): the rows FSFS records for the transaction `svnadmin load`
//! makes, the texts and lists the repository does not hold yet, and exactly the stream's revision properties,
//! `svn:date` and its absence among them, as `load-fs-vtable.c` `close_revision` keeps them.
//!
//! The step's refusals ([`StepRefusal`]) are typed and answered with Subversion's numbers; a refused revision changes
//! nothing. A revision whose head is no longer the youngest is refused as FSFS's `commit_body` refuses a transaction
//! whose base moved, "Transaction out of date" (`transaction.c` 3784-3786); the owner's step first judges such a work
//! again at the head by position, as `svn_fs_fs__commit_txn` merges again (`tree.c` 2295-2310; A-655 part 2b over
//! A-658), so a client is refused only where the verdict finds a conflict.
//!
//! No I/O, no clock (the caller's instant), no randomness, no panic.

use std::collections::BTreeMap;

use super::drive::Work;
use crate::index::{self, ContentIdentity, IndexRefusal, RevisionRecord};
use crate::lock::{AprTime, LockEntry, LockRefusal};
use crate::migration::Properties;
use crate::migration::properties::encode;

/// The revision property a commit's date is.
/// Format: `SVN_PROP_REVISION_DATE` (`svn_props.h`).
const REVISION_DATE: &[u8] = b"svn:date";

/// The revision property a commit's author is.
/// Format: `SVN_PROP_REVISION_AUTHOR` (`svn_props.h`).
const REVISION_AUTHOR: &[u8] = b"svn:author";

/// `SVN_ERR_FS_GENERAL`, "General filesystem error".
/// Format: the filesystem category's start, 120,000 + 8 × 5,000 = 160,000, plus 0 (`svn_error_codes.h` 581-583).
const SVN_ERR_FS_GENERAL: u32 = 160_000;

/// `SVN_ERR_FS_CORRUPT`, "Filesystem is corrupt".
/// Format: the filesystem category's start plus 4 (`svn_error_codes.h` 597-599).
const SVN_ERR_FS_CORRUPT: u32 = 160_004;

/// `SVN_ERR_FS_TXN_OUT_OF_DATE`, which FSFS's commit writes "Transaction out of date" (`transaction.c` 3785-3786).
/// Format: the filesystem category's start plus 28 (`svn_error_codes.h` 693-695).
const SVN_ERR_FS_TXN_OUT_OF_DATE: u32 = 160_028;

/// FSFS's words for a transaction whose base is no longer the youngest revision.
/// Format: `transaction.c` `commit_body` 3785-3786, `_("Transaction out of date")`.
const TRANSACTION_OUT_OF_DATE: &str = "Transaction out of date";

/// `SVN_ERR_FS_GENERAL`'s own words, which `svn_err_best_message` gives an error made without any.
/// Format: `svn_error_codes.h` 581-583.
const GENERAL_FILESYSTEM_ERROR: &str = "General filesystem error";

/// `SVN_ERR_FS_CORRUPT`'s own words.
/// Format: `svn_error_codes.h` 597-599.
const FILESYSTEM_IS_CORRUPT: &str = "Filesystem is corrupt";

/// One revision as the owner commits it (the module documentation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
  /// The youngest revision it was judged against (a client's work: the head its verdict merged, which FSFS makes its
  /// transaction's base, `tree.c` 2291) or made on (an import's): it commits as the revision after, and only while
  /// this is still the youngest.
  pub head: index::Revision,
  /// The transaction's name and the changed-path rows.
  pub record: RevisionRecord,
  /// The revision properties, `svn:date` among them for a client's commit.
  pub properties: Properties,
  /// The texts and property lists the rows name, by content identity: a text's bytes, a property list in FSFS's form
  /// (`hash.c` `svn_hash_write2`). The step keeps those its repository does not hold.
  pub contents: BTreeMap<ContentIdentity, Vec<u8>>,
  /// The lock entries its verdict decided, applied with it: the presented locks a commit releases and the expired
  /// locks it met (A-602); none for an import.
  pub lock_entries: Vec<LockEntry>,
}

impl Revision {
  /// A client's accepted work as the revision it commits, dated `now`, the owner's instant at the step (the module
  /// documentation).
  ///
  /// # Errors
  /// [`StepRefusal::Date`] for an instant this writer cannot write as `svn_time_to_cstring` does: before 1970 or past
  /// the year 9999, which no owner's clock reads.
  pub fn of_work(work: Work, now: AprTime) -> Result<Self, StepRefusal> {
    let date = crate::dav::response::svn_time(now).map_err(|_| StepRefusal::Date { now })?;
    let mut properties: Properties = work.revision_properties.into_iter().collect();
    properties.insert(REVISION_DATE.to_vec(), date.into_bytes());
    let mut contents = work.texts;
    for (identity, list) in work.property_lists {
      let list: Properties = list.into_iter().collect();
      contents.insert(identity, encode(&list));
    }
    Ok(Self {
      head: work.head,
      record: work.record,
      properties,
      contents,
      lock_entries: work.lock_entries,
    })
  }

  /// Its `svn:date`, as `commit-info` carries it; none when it holds none or one that is not UTF-8.
  pub fn date(&self) -> Option<&str> {
    self.text_property(REVISION_DATE)
  }

  /// Its `svn:author`, as `commit-info` carries it; none for an anonymous commit.
  pub fn author(&self) -> Option<&str> {
    self.text_property(REVISION_AUTHOR)
  }

  /// The revision property `name` read as UTF-8.
  fn text_property(&self, name: &[u8]) -> Option<&str> {
    self
      .properties
      .get(name)
      .and_then(|value| std::str::from_utf8(value).ok())
  }
}

/// Why the commit step refused a revision, each answered with a Subversion error ([`StepRefusal::subversion_code`],
/// [`StepRefusal::message`]). A refused revision changes nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepRefusal {
  /// The repository's youngest revision is past the revision's head, or moved since the revision was checked: FSFS's
  /// commit of a transaction whose base is no longer the youngest (`SVN_ERR_FS_TXN_OUT_OF_DATE`, "Transaction out of
  /// date").
  OutOfDate {
    /// The revision's head.
    head: index::Revision,
    /// The repository's youngest revision.
    youngest: index::Revision,
  },
  /// The rows, refused by the index, with its own number.
  Index(IndexRefusal),
  /// A lock entry the lock table does not take: a lock the verdict released, removed or replaced since (A-602's
  /// refusal, `SVN_ERR_FS_CORRUPT`).
  Lock(LockRefusal),
  /// Records past the room their repository is held within (`SVN_ERR_FS_GENERAL`: Subversion has no closer error).
  Room {
    /// The bytes they would hold.
    needed: u64,
    /// The room's bytes.
    room: u64,
  },
  /// A text or property list the rows name that neither the repository nor the revision holds, or a count past
  /// 2^64 - 1 (`SVN_ERR_FS_CORRUPT`).
  Corrupt,
  /// An owner's instant this writer cannot write as `svn_time_to_cstring` does (`SVN_ERR_FS_GENERAL`).
  Date {
    /// The instant.
    now: AprTime,
  },
}

impl StepRefusal {
  /// The number of the Subversion error a client is answered with.
  pub fn subversion_code(&self) -> u32 {
    match self {
      Self::OutOfDate { .. } => SVN_ERR_FS_TXN_OUT_OF_DATE,
      Self::Index(refusal) => refusal.subversion_code(),
      Self::Lock(refusal) => refusal.subversion_code(),
      Self::Room { .. } | Self::Date { .. } => SVN_ERR_FS_GENERAL,
      Self::Corrupt => SVN_ERR_FS_CORRUPT,
    }
  }

  /// The words a client is answered with: FSFS's where FSFS has a refusal of its own; the error's own words, as
  /// `svn_err_best_message` gives an error made without any; and an index's or a lock table's refusal, which no
  /// stock client meets once its verdict accepted, in its own terms, as the drive words one.
  pub fn message(&self) -> String {
    match self {
      Self::OutOfDate { .. } => TRANSACTION_OUT_OF_DATE.to_owned(),
      Self::Room { .. } | Self::Date { .. } => GENERAL_FILESYSTEM_ERROR.to_owned(),
      Self::Corrupt => FILESYSTEM_IS_CORRUPT.to_owned(),
      Self::Index(refusal) => format!("{refusal:?}"),
      Self::Lock(refusal) => format!("{refusal:?}"),
    }
  }
}

impl From<IndexRefusal> for StepRefusal {
  fn from(refusal: IndexRefusal) -> Self {
    match refusal {
      IndexRefusal::OutOfDate { planned, next } => Self::OutOfDate {
        head: planned.saturating_sub(1),
        youngest: next.saturating_sub(1),
      },
      other => Self::Index(other),
    }
  }
}

#[cfg(test)]
mod tests;
