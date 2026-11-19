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
//!   the search began, the copy with the start as it moves, as the C code compares them.
//! - Found, the answer is `( success ( rev ) )`. Not found, `svnserve` answers the refusal a newer client reads as no
//!   revision: `SVN_ERR_ENTRY_MISSING_REVISION`, its message naming the path and the two revisions as given.
//!
//! The search halves its range each turn, so it ends within [`SEARCH_TURNS`] turns; a search that would not (an index
//! that contradicts itself) is refused as a corrupt filesystem, never looped on.

use super::{
  Failure, Outcome, Reads, Repository, SVN_ERR_FS_NO_SUCH_REVISION, c_string, full_path, revnum,
};
use crate::index::{Node, Revision};
use crate::ra_svn::{ItemError, Value};

/// `SVN_ERR_ENTRY_MISSING_REVISION`, the refusal `svnserve` answers for a path not deleted in the range.
/// Format: the entry category's start, 120,000 + 6 × 5,000 = 150,000, plus 3 (`svn_error_codes.h`); recorded (fixture
/// `get-deleted-rev`).
const SVN_ERR_ENTRY_MISSING_REVISION: u32 = 150_003;

/// The most turns the search can take.
/// Derived: each turn halves the range between the start and the end, and a range of revisions is at most 2^64 wide,
/// so the search ends within 64 turns and one more for the turn that finds it.
const SEARCH_TURNS: u32 = u64::BITS.saturating_add(1);

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

  /// `svn_repos_deleted_rev`: the revision the node at `path` in the older revision was first deleted in, at or before
  /// the younger; `None` when it was not.
  fn deletion(&self, path: &str, start: i64, end: i64) -> Result<Option<Revision>, Failure> {
    if start < 0 {
      let message = format!("Invalid start revision {start}");
      return Err(Failure::new(SVN_ERR_FS_NO_SUCH_REVISION, message));
    }
    if end < 0 {
      let message = format!("Invalid end revision {end}");
      return Err(Failure::new(SVN_ERR_FS_NO_SUCH_REVISION, message));
    }
    let (start, end) = (start.min(end), start.max(end));
    let start = self.existing(start)?;
    let Some(first) = self.node(path, start)? else {
      return Ok(None);
    };
    let end = self.existing(end)?;
    if let Some(last) = self.node(path, end)?
      && last.node_id() == first.node_id()
      && !self.copied_after(path, end, start)?
    {
      return Ok(None);
    }
    self.search(path, &first, start, end).map(Some)
  }

  /// Whether the closest copy of `path` in `revision` was made after `start`.
  fn copied_after(&self, path: &str, revision: Revision, start: Revision) -> Result<bool, Failure> {
    Ok(
      self
        .closest_copy(path, revision)?
        .is_some_and(|(copied_in, _)| copied_in > start),
    )
  }

  /// `svn_repos_deleted_rev`'s binary search between `start`, where `first` stands, and `end`, where it does not.
  fn search(
    &self,
    path: &str,
    first: &Node<'_>,
    mut start: Revision,
    mut end: Revision,
  ) -> Result<Revision, Failure> {
    let mut middle = midpoint(start, end);
    for _ in 0..SEARCH_TURNS {
      let lower = match self.node(path, middle)? {
        None => true,
        Some(node) => node.node_id() != first.node_id() || self.copied_after(path, middle, start)?,
      };
      if lower {
        end = middle;
        middle = midpoint(start, middle);
      } else if end.saturating_sub(middle) == 1 {
        return Ok(end);
      } else {
        start = middle;
        middle = midpoint(start, end);
      }
    }
    Err(Failure::corrupt())
  }
}

/// `(start + end) / 2` without overflow, for `start <= end`.
fn midpoint(start: Revision, end: Revision) -> Revision {
  start.saturating_add(end.saturating_sub(start) / 2)
}
