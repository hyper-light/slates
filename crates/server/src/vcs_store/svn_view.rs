//! A Subversion repository's records, lent from A-662's map on its owner shard (A-681; A-655 part 2b; §4.17.4 item 5
//! "HTTP", item 10 and piece 13): the [`SvnStores`] the DAV site reads a repository through (`crate::vcs_http::svn`),
//! and [`lend_in`], which the svn:// listener's connection takes its sources from (A-689), each made afresh for one
//! step from what the shard holds then, so nothing of the shard is held across a wait, as git's and Mercurial's sites
//! are lent theirs.
//!
//! **What is lent.** The repository's records (`slates_svn::migration::Repository`, from its store in the map,
//! [`super::VcsStores::subversion_records`]): the Subversion index; every revision's properties; every text and property
//! list, held once by its content identity; the lock table; and the UUID as text. The records are the read source
//! (`slates_svn::reads::Repository` and `slates_svn::mergeinfo::Contents`): a directory's entries from the index
//! (`Index::children`), a property list decoded in place, a text from an offset to its end. And the owner's instant,
//! its clock's wall time, at which a lock's expiry is judged (`lock.c` `lock_expired`).
//!
//! **Read rules.** A repository's record names no Subversion authorization file yet, so no rule narrows a principal's
//! reads below what the repository's policy lets it read (`crate::vcs_policy::reaches`, asked before anything is lent),
//! as `svnserve` and `mod_authz_svn` read every path of a repository whose configuration names no `authz-db` (GAPS).
//!
//! **Not lent.** A repository whose records this shard does not hold: one of another tool, one on another shard, or
//! one whose creation's record has not been replayed. The site answers it `503`.

use slates_db::catalog::{Principal, RepositoryId};
use slates_svn::lock::AprTime;
use slates_svn::migration::Repository as SvnRecords;
use slates_vfs::clock::Clock;

use crate::state::ShardState;
use crate::vcs_http::svn::{Lent, SvnStores};

/// The daemon's lender (module docs): the records of the Subversion repositories this shard's map holds.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ServedRecords;

impl SvnStores for ServedRecords {
  type Repository = SvnRecords;

  fn holds(&self, id: RepositoryId) -> bool {
    crate::state::with_state(|state| state.vcs_stores.subversion_records(id).is_some())
      .unwrap_or(false)
  }

  fn lend<T>(
    &mut self,
    id: RepositoryId,
    _principal: &Principal,
    step: impl FnOnce(&Lent<'_, SvnRecords>) -> T,
  ) -> Option<T> {
    crate::state::with_state(|state| lend_in(state, id, step)).flatten()
  }
}

/// Runs `step` over repository `id`'s records in `state` as they stand, with the owner's instant (module docs); none
/// when `state` holds none.
pub(crate) fn lend_in<T>(
  state: &mut ShardState,
  id: RepositoryId,
  step: impl FnOnce(&Lent<'_, SvnRecords>) -> T,
) -> Option<T> {
  let now = AprTime::from_wall_ns(state.clock.wall_ns());
  let records = state.vcs_stores.subversion_records(id)?;
  let uuid = records.uuid();
  // A record's UUID is RFC 4122's text, ASCII alone (`Uuid::parse`), so it always reads as UTF-8.
  let uuid = std::str::from_utf8(uuid.as_bytes()).ok()?;
  Some(step(&Lent {
    index: records.index(),
    repository: records,
    uuid,
    locks: records.locks(),
    rules: None,
    now,
  }))
}
