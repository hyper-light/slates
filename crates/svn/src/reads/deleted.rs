//! `get-deleted-rev` (§4.17.4 "4. Reads": `get-deleted-rev` reads the history lists and copy sources; A-613 part 2):
//! the revision in which the node at a path in one revision was first deleted, at or before another, as `svnserve`'s
//! `get_deleted_rev` and `svn_repos_deleted_rev` answer it (`serve.c`, `libsvn_repos/rev_hunt.c`, Subversion 1.15.0),
//! held to stock `svnserve`'s transcripts (`crates/svn/fixtures/reads`, session `get-deleted-rev`):
//! - The parameters are `crr`: the path, the peg and the end revision, both required; then the authentication request.
//! - A negative revision is refused, "Invalid start revision N" or "Invalid end revision N" (160006); the two are taken
//!   in order, the older first; a revision past the youngest is refused when it is opened ("No such revision N").
//! - A path absent in the older revision was never deleted in the range. Present there and in the younger revision, a
//!   node of the same line that no copy younger than the older revision made is not deleted either.
//! - Otherwise the search is `svn_repos_deleted_rev`'s binary search: below a middle revision where the path is absent,
//!   holds another line, or holds the line through a copy younger than the start (the start the search has moved to);
//!   above it otherwise; found when the end is one past the middle. The node is compared with the older revision's as
//!   the search began, the copy with the start as it moves, as the C code compares them. The search, and the closest
//!   copy it asks, are the crate's one walk of copy roots (`crate::report::drive::history`, A-616), which the drive
//!   asks too.
//! - Found, the answer is `( success ( rev ) )`. Not found, `svnserve` answers the refusal a newer client reads as no
//!   revision: `SVN_ERR_ENTRY_MISSING_REVISION`, its message naming the path and the two revisions as given.
//!
//! The search halves its range each turn, so it ends within one turn a bit of a revision number and one more; the walk
//! answers none past that rather than loop (an index that contradicts itself).

use super::{
  Failure, Outcome, Reads, Repository, SVN_ERR_FS_NO_SUCH_REVISION, c_string, full_path, revnum,
};
use crate::index::Revision;
use crate::ra_svn::{ItemError, Value};
use crate::report::drive::history as copies;

/// `SVN_ERR_ENTRY_MISSING_REVISION`, the refusal `svnserve` answers for a path not deleted in the range.
/// Format: the entry category's start, 120,000 + 6 × 5,000 = 150,000, plus 3 (`svn_error_codes.h`); recorded (fixture
/// `get-deleted-rev`).
const SVN_ERR_ENTRY_MISSING_REVISION: u32 = 150_003;

impl<R: Repository + ?Sized> Reads<'_, R> {
  /// `get-deleted-rev ( path peg-rev end-rev )` (`serve.c` `get_deleted_rev`).
  pub(super) fn deleted_revision(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let (Some(path), Some(peg), Some(end)) = (
      c_string(params.first()),
      revnum(params.get(1)),
      revnum(params.get(2)),
    ) else {
      return Ok(Outcome::Close);
    };
    let path = full_path(self.base, path);
    self.trivial_auth()?;
    match self.deletion(&path, peg, end) {
      Ok(Some(revision)) => self.success(vec![Value::Number(revision)]),
      Ok(None) => self.failure(&Failure::new(
        SVN_ERR_ENTRY_MISSING_REVISION,
        format!(
          "svn protocol command 'get-deleted-rev': path '{path}' was not deleted in r{peg}-{end}; NOTE: newer clients \
           handle this case and do not report it as an error"
        ),
      )),
      Err(failure) => self.failure(&failure),
    }
  }

  /// `svn_repos_deleted_rev`: its refusals of a negative revision here, then the search over the crate's one walk of
  /// copy roots (`crate::report::drive::history::deleted_revision`); `None` when the path was not deleted.
  fn deletion(&self, path: &str, start: i64, end: i64) -> Result<Option<Revision>, Failure> {
    if start < 0 {
      let message = format!("Invalid start revision {start}");
      return Err(Failure::new(SVN_ERR_FS_NO_SUCH_REVISION, message));
    }
    if end < 0 {
      let message = format!("Invalid end revision {end}");
      return Err(Failure::new(SVN_ERR_FS_NO_SUCH_REVISION, message));
    }
    let revision = |revision: i64| Revision::try_from(revision).map_err(|_| Failure::corrupt());
    copies::deleted_revision(self.index, path, revision(start)?, revision(end)?)
      .map_err(|refusal| Failure::of_index(&refusal))
  }
}
