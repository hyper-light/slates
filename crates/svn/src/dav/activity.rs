//! DeltaV v1's commit resources (A-766 part 2; `mod_dav_svn` 1.14.x `repos.c`, `activity.c`, `version.c`): what a
//! client without HTTPv2 (svn before 1.7, or 1.7 and later where OPTIONS has no v2 headers) commits through, read from
//! a request's path and written as the URLs `mod_dav_svn` gives them.
//!
//! **The flow** (ra_serf 1.14 `commit.c`, the non-HTTPv2 branch of `open_root`): MKACTIVITY of `!svn/act/ID` (an id the
//! client makes, a UUID); a PROPFIND of the VCC for its `checked-in` baseline; a CHECKOUT of the VCC naming the
//! activity, answered with the working baseline `!svn/wbl/ID/REV` (`dav_svn__create_working_resource`, baselined),
//! whose PROPPATCH carries the revision properties; a CHECKOUT of each version resource `!svn/ver/REV/PATH` it changes,
//! answered with the working resource `!svn/wrk/ID/PATH`; PUT, MKCOL, DELETE, COPY and PROPPATCH on working resources;
//! a MERGE whose source is the activity; and a DELETE of the activity. Each maps onto HTTPv2's transaction
//! ([`super::commit::DavCommit`]): the activity is the transaction, a working resource its root's path, and a CHECKOUT's
//! revision the base revision HTTPv2 sends as `X-SVN-Version-Name`.
//!
//! No I/O, no clock, no panic.

use super::drive::uri_encode;
use super::site::SPECIAL;
use crate::index::Revision;

/// The special resource of an activity.
/// Format: `repos.c` `special_subdirs`, `act`.
const ACTIVITY: &str = "act";

/// The special resource of a working baseline.
/// Format: `repos.c` `special_subdirs`, `wbl`.
const WORKING_BASELINE: &str = "wbl";

/// The special resource of a working resource.
/// Format: `repos.c` `special_subdirs`, `wrk`.
const WORKING: &str = "wrk";

/// A DeltaV v1 commit resource a request's path names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum V1Resource<'s> {
  /// An activity, `!svn/act/ID`.
  Activity(&'s str),
  /// A working baseline, `!svn/wbl/ID/REV`: the activity's baseline, where its revision properties are set.
  WorkingBaseline {
    /// The activity.
    activity: &'s str,
    /// The baseline's revision.
    revision: Revision,
  },
  /// A working resource, `!svn/wrk/ID/PATH`: the node at `PATH` in the activity's transaction.
  Working {
    /// The activity.
    activity: &'s str,
    /// The repository path, canonical (`/` for the root).
    path: String,
  },
}

/// The DeltaV v1 commit resource `segments` (a request's path below the repository's root, decoded, split at `/`)
/// name, if any (`repos.c` `parse_activity_uri`, `parse_wrk_uri`, `parse_working_baseline_uri`); none for every other
/// path and for a malformed one, which the read side answers.
pub fn v1_resource<'s>(segments: &[&'s str]) -> Option<V1Resource<'s>> {
  let (first, rest) = segments.split_first()?;
  if *first != SPECIAL {
    return None;
  }
  match rest {
    [kind, activity] if *kind == ACTIVITY => Some(V1Resource::Activity(activity)),
    [kind, activity, revision] if *kind == WORKING_BASELINE => Some(V1Resource::WorkingBaseline {
      activity,
      revision: decimal(revision)?,
    }),
    [kind, activity, path @ ..] if *kind == WORKING => Some(V1Resource::Working {
      activity,
      path: joined(path),
    }),
    _ => None,
  }
}

/// The working baseline's URL path for `activity` at `revision` below the repository's `root`, URI-encoded as
/// `dav_svn__create_working_resource` builds it (`/ROOT/!svn/wbl/ID/REV`).
pub fn working_baseline_uri(root: &str, activity: &str, revision: Revision) -> String {
  format!(
    "{}/{SPECIAL}/{WORKING_BASELINE}/{}/{revision}",
    root.trim_end_matches('/'),
    uri_encode(activity)
  )
}

/// The working resource's URL path for `path` in `activity` below the repository's `root`, URI-encoded as
/// `dav_svn__create_working_resource` builds it (`/ROOT/!svn/wrk/ID/PATH`; the root's is `.../wrk/ID/`).
pub fn working_uri(root: &str, activity: &str, path: &str) -> String {
  format!(
    "{}/{SPECIAL}/{WORKING}/{}{}",
    root.trim_end_matches('/'),
    uri_encode(activity),
    uri_encode(path)
  )
}

/// A revision written in decimal digits alone.
fn decimal(text: &str) -> Option<Revision> {
  if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
    return None;
  }
  text.parse().ok()
}

/// The canonical repository path of `segments`: `/` for none.
fn joined(segments: &[&str]) -> String {
  if segments.is_empty() {
    return String::from("/");
  }
  segments.iter().fold(String::new(), |mut path, name| {
    path.push('/');
    path.push_str(name);
    path
  })
}

#[cfg(test)]
mod tests;
