//! `get-lock` and `get-locks` (`serve.c` 3366-3446, Subversion 1.15.0; A-693 part 1): the lock table's locks
//! (A-602, the one record A-656 continued (3) made it), read at the owner's instant, written as `write_lock` writes
//! one (`ccc(?c)c(?c)`: path, token, owner, the comment if any, the creation date and the expiry if any, each date by
//! `svn_time_to_cstring`).
//!
//! `get-lock ( path:string )` checks the path as every read does (`must_have_access`, [`Reads::must_read`]), then
//! answers `( success ( ( [lock] ) ) )`. `get-locks ( path:string [ ( depth:word ) ] )` refuses a depth that is not
//! `empty`, `files`, `immediates` or `infinity` (none given is `infinity`) "Invalid 'depth' specified in get-locks
//! request" (200004) and goes on; otherwise it sends the trivial authentication request and the locks reached at
//! that depth whose paths the session may read (`svn_repos_fs_get_locks2` with `authz_check_access_cb_func`), in path
//! order where `svnserve` sends its hash's.

use super::{Failure, Outcome, Reads, Repository, full_path};
use crate::lock::{AprTime, Depth, Lock, LockTable};
use crate::ra_svn::{ItemError, Value};

/// `SVN_ERR_INCORRECT_PARAMS` (`svn_error_codes.h`: `SVN_ERR_MISC_CATEGORY_START` + 4).
const SVN_ERR_INCORRECT_PARAMS: u32 = 200_004;

/// `get_locks`' refusal of a depth it does not take, in its words.
const INVALID_DEPTH: &str = "Invalid 'depth' specified in get-locks request";

/// A lock's dates as `svn_time_to_cstring` writes them: created, and the expiry if any.
type LockDates = (String, Option<String>);

impl<R: Repository + ?Sized> Reads<'_, R> {
  /// `get-lock` (module documentation).
  pub(super) fn get_lock(
    &mut self,
    params: &[Value<'_>],
    locks: &LockTable,
    now: AprTime,
  ) -> Result<Outcome, ItemError> {
    let [Value::String(path), ..] = params else {
      return Ok(Outcome::Close);
    };
    let path = full_path(self.base, path);
    if let Some(outcome) = self.must_read(&path)? {
      return Ok(outcome);
    }
    let held: Vec<&Lock> = locks.get_lock(&path, now).into_iter().collect();
    self.locks_answer(&held)
  }

  /// `get-locks` (module documentation).
  pub(super) fn get_locks(
    &mut self,
    params: &[Value<'_>],
    locks: &LockTable,
    now: AprTime,
  ) -> Result<Outcome, ItemError> {
    let (path, depth) = match params {
      [Value::String(path)] => (path, Some(Depth::Infinity)),
      [Value::String(path), Value::List(depth)] => match depth.as_slice() {
        [] => (path, Some(Depth::Infinity)),
        [Value::Word(word)] => (path, depth_of(word)),
        _ => return Ok(Outcome::Close),
      },
      _ => return Ok(Outcome::Close),
    };
    let Some(depth) = depth else {
      return self.failure(&Failure::new(
        SVN_ERR_INCORRECT_PARAMS,
        String::from(INVALID_DEPTH),
      ));
    };
    let path = full_path(self.base, path);
    self.trivial_auth()?;
    let mut held: Vec<&Lock> = locks
      .get_locks(&path, depth, now)
      .into_iter()
      .filter(|lock| self.readable(&lock.path))
      .collect();
    held.sort_by(|left, right| left.path.cmp(&right.path));
    self.locks_answer(&held)
  }

  /// `( success ( ( lock ... ) ) )`: `w((!`, each lock by `write_lock`, then `!))`.
  fn locks_answer(&mut self, held: &[&Lock]) -> Result<Outcome, ItemError> {
    let dates: Vec<LockDates> = held
      .iter()
      .map(|lock| {
        (
          super::format_time(lock.created.0),
          lock.expires.map(|at| super::format_time(at.0)),
        )
      })
      .collect();
    let written = held
      .iter()
      .zip(&dates)
      .map(|(lock, dates)| lock_value(lock, dates))
      .collect();
    self.success(vec![Value::List(written)])
  }
}

/// `lock` as `write_lock` writes it, handed to `with` as one value (A-693 part 2's lock verbs answer with it).
pub(crate) fn with_lock_value<T>(lock: &Lock, with: impl FnOnce(Value<'_>) -> T) -> T {
  let dates = (
    super::format_time(lock.created.0),
    lock.expires.map(|at| super::format_time(at.0)),
  );
  with(lock_value(lock, &dates))
}

/// One lock as `write_lock` writes it: `( path token owner ( [comment] ) created ( [expires] ) )`.
fn lock_value<'v>(lock: &'v Lock, (created, expires): &'v LockDates) -> Value<'v> {
  Value::List(vec![
    Value::String(lock.path.as_bytes()),
    Value::String(lock.token.as_str().as_bytes()),
    Value::String(lock.owner.as_bytes()),
    Value::List(
      lock
        .comment
        .iter()
        .map(|comment| Value::String(comment.as_bytes()))
        .collect(),
    ),
    Value::String(created.as_bytes()),
    Value::List(
      expires
        .iter()
        .map(|at| Value::String(at.as_bytes()))
        .collect(),
    ),
  ])
}

/// The depths `get-locks` takes (`svn_depth_from_word`, then `get_locks`' check): none for any other word.
fn depth_of(word: &str) -> Option<Depth> {
  match word {
    "empty" => Some(Depth::Empty),
    "files" => Some(Depth::Files),
    "immediates" => Some(Depth::Immediates),
    "infinity" => Some(Depth::Infinity),
    _ => None,
  }
}
