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
//! **The tunnel's lender** ([`TunnelRecords`], `svnserve -t` over the daemon's SSH site, A-688): the same records lent
//! as the tunnel's sources, opened with the UUID as the realm and `EXTERNAL` alone (the SSH key proved the principal,
//! whose reach the tunnel judged before opening), the session writing where it reaches, and its accepted work landed
//! through the owner's commit step (`repository::svn_commit::commit_work`, A-698).
//!
//! **Not lent.** A repository whose records this shard does not hold: one of another tool, one on another shard, or
//! one whose creation's record has not been replayed. The site answers it `503`.

use slates_db::catalog::{Principal, RepositoryId};
use slates_svn::lock::{AprTime, LockEntry, UUID_BYTES};
use slates_svn::migration::Repository as SvnRecords;
use slates_vfs::clock::Clock;

use slates_svn::commit::drive::{Due, Setup, Work};
use slates_svn::dav::commit::{DavCommit, World as CommitWorld};
use slates_svn::dav::locking::HttpAnswer;
use slates_svn::serve::{Policy, Sources};

use crate::repository::svn_commit::{self, Committed};
use crate::state::ShardState;
use crate::vcs_http::svn::{Lent, SvnStores};
use crate::vcs_ssh::svn::Opening;

/// Counter: an HTTP lock verb whose new token the system's generator could not draw: its landing refused.
const TOKEN_UNDRAWN: &str = "vcs_http.svn_token_undrawn";

/// The daemon's lender (module docs): the records of the Subversion repositories this shard's map holds.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ServedRecords;

impl SvnStores for ServedRecords {
  type Repository = SvnRecords;

  type Hold = SvnHold;

  fn fetch_quantum(&self) -> Option<u64> {
    crate::state::with_state(|state| crate::vcs_store::hg_import::quantum_of(state))
  }

  fn hold(&mut self, id: RepositoryId) -> Option<SvnHold> {
    crate::state::with_state(|state| hold_in(state, id)).flatten()
  }

  fn lend_held<T>(
    &mut self,
    hold: &SvnHold,
    _principal: &Principal,
    step: impl FnOnce(&Lent<'_, SvnRecords>) -> T,
  ) -> Option<T> {
    crate::state::with_state(|state| lend_held_in(state, hold, step)).flatten()
  }

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

  fn begin_commit(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    setup_of: impl FnOnce(String, AprTime) -> Setup,
  ) -> Option<Result<(String, Option<Due>), HttpAnswer>> {
    crate::state::with_state(|state| {
      let now = AprTime::from_wall_ns(state.clock.wall_ns());
      let youngest = state.vcs_stores.subversion_records(id)?.index().youngest();
      let name = state.vcs_stores.dav_transaction_name(youngest);
      let records = state.vcs_stores.subversion_records(id)?;
      let world = CommitWorld {
        index: records.index(),
        green: records,
        locks: records.locks(),
      };
      // The commit's sink: in memory until the green's staged blocks take its texts (w-perf's wiring of A-716).
      let texts = Box::new(slates_svn::commit::drive::HeldTexts::new());
      let begun = DavCommit::begin_hooked(setup_of(name.clone(), now), &world, texts);
      Some(begun.map(|(commit, due)| {
        let key = principal.key();
        let now_ns = state.clock.monotonic_ns();
        state
          .vcs_stores
          .begin_dav_commit(id, &key, name.clone(), (commit, now_ns));
        (name, due)
      }))
    })
    .flatten()
  }

  fn on_commit<T>(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    name: &str,
    step: impl FnOnce(&mut DavCommit, &CommitWorld<'_, SvnRecords>) -> T,
  ) -> Option<Option<T>> {
    crate::state::with_state(|state| {
      let key = principal.key();
      state.vcs_stores.subversion_records(id)?;
      let now_ns = state.clock.monotonic_ns();
      let Some(mut commit) = state.vcs_stores.take_dav_commit(id, &key, (name, now_ns)) else {
        return Some(None);
      };
      let records = state.vcs_stores.subversion_records(id)?;
      let world = CommitWorld {
        index: records.index(),
        green: records,
        locks: records.locks(),
      };
      let stepped = step(&mut commit, &world);
      let now_ns = state.clock.monotonic_ns();
      state
        .vcs_stores
        .keep_dav_commit(id, &key, name, (commit, now_ns));
      Some(Some(stepped))
    })
    .flatten()
  }

  fn land(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    work: Work,
    texts: &dyn slates_svn::commit::drive::TextSink,
  ) -> Option<Result<Committed, svn_commit::Uncommitted>> {
    crate::state::with_state(|state| {
      let now = AprTime::from_wall_ns(state.clock.wall_ns());
      svn_commit::commit_work(state, (principal, id), work, now, texts)
    })
  }
  fn lock_verb(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    judge: impl FnOnce(
      &Lent<'_, SvnRecords>,
      &mut dyn FnMut() -> [u8; UUID_BYTES],
    ) -> (Vec<LockEntry>, HttpAnswer),
  ) -> Option<Result<HttpAnswer, svn_commit::Uncommitted>> {
    crate::state::with_state(|state| {
      let undrawn = std::cell::Cell::new(false);
      let mut draw = || {
        let mut token = [0u8; UUID_BYTES];
        if aws_lc_rs::rand::fill(&mut token).is_err() {
          undrawn.set(true);
        }
        token
      };
      let (entries, answer) = lend_in(state, id, |lent| judge(lent, &mut draw))?;
      if undrawn.get() {
        crate::fleet::count_refusal(TOKEN_UNDRAWN);
        return Some(Err(svn_commit::Uncommitted::NotHeld));
      }
      if entries.is_empty() {
        return Some(Ok(answer));
      }
      Some(
        svn_commit::record(
          state,
          (principal, id),
          slates_svn::commit::record::Record::Locks(entries),
        )
        .map(|()| answer),
      )
    })
    .flatten()
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
  lent_of(records, now, step)
}

/// A fetch's hold on the generation of a Subversion repository's records it began on (A-749, §4.17.1 "A fetch's answer"
/// rule 4, `super::hold_svn`): every slice of its answer reads those records, whatever rebuilds or removes the store
/// meanwhile. Given back by its drop, on the shard that holds the store.
#[derive(Debug)]
pub(crate) struct SvnHold {
  /// The repository.
  id: RepositoryId,
  /// The generation held, until it is given back.
  generation: Option<u64>,
}

impl SvnHold {
  /// Gives the hold back on `state`, for a caller that holds the shard's state (the tests), which its drop cannot reach.
  #[cfg(test)]
  pub(crate) fn give_back(mut self, state: &mut ShardState) {
    if let Some(generation) = self.generation.take() {
      super::release_svn_hold(state, generation);
    }
  }
}

impl Drop for SvnHold {
  fn drop(&mut self) {
    if let Some(generation) = self.generation.take() {
      crate::state::with_state(|state| super::release_svn_hold(state, generation))
        .unwrap_or_default();
    }
  }
}

/// Holds repository `id`'s records as `state` holds them now ([`SvnHold`]); none when they are not lent here.
pub(crate) fn hold_in(state: &mut ShardState, id: RepositoryId) -> Option<SvnHold> {
  let generation = super::hold_svn(&mut state.vcs_stores, id)?;
  Some(SvnHold {
    id,
    generation: Some(generation),
  })
}

/// Runs `step` over the records `hold` holds, as [`lend_in`] lends a repository's own; none when they are gone.
pub(crate) fn lend_held_in<T>(
  state: &mut ShardState,
  hold: &SvnHold,
  step: impl FnOnce(&Lent<'_, SvnRecords>) -> T,
) -> Option<T> {
  let now = AprTime::from_wall_ns(state.clock.wall_ns());
  let records = super::held_svn_records(&state.vcs_stores, hold.id, hold.generation?)?;
  lent_of(records, now, step)
}

/// Runs `step` over `records` lent at `now`.
fn lent_of<T>(
  records: &SvnRecords,
  now: AprTime,
  step: impl FnOnce(&Lent<'_, SvnRecords>) -> T,
) -> Option<T> {
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

/// The `svn+ssh://` tunnel's lender (module docs, "The tunnel's lender"): the records of the Subversion repositories
/// this shard's map holds.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TunnelRecords;

/// Whether a repository keeps mergeinfo, which `repos-info` announces: every one does, as an FSFS repository of a
/// format that records mergeinfo announces it (`svn_repos_has_capability`).
const MERGEINFO_KEPT: bool = true;

impl crate::vcs_ssh::svn::SvnStores for TunnelRecords {
  type Green = SvnRecords;

  fn holds(&self, id: RepositoryId) -> bool {
    crate::state::with_state(|state| state.vcs_stores.subversion_records(id).is_some())
      .unwrap_or(false)
  }

  fn opening<T>(&mut self, id: RepositoryId, step: impl FnOnce(&Opening<'_>) -> T) -> Option<T> {
    crate::state::with_state(|state| {
      lend_in(state, id, |lent| {
        step(&Opening {
          uuid: lent.uuid.as_bytes(),
          realm: lent.uuid.as_bytes(),
          mergeinfo: MERGEINFO_KEPT,
          anonymous: false,
          authenticated: true,
          cram_md5: false,
        })
      })
    })
    .flatten()
  }

  fn lend<T>(
    &mut self,
    id: RepositoryId,
    texts: &dyn slates_svn::commit::drive::TextSink,
    step: impl FnOnce(&Sources<'_, SvnRecords>) -> T,
  ) -> Option<T> {
    crate::state::with_state(|state| {
      let name = state
        .db
        .partition()
        .repositories()
        .repository(id)
        .map(|record| record.name.clone())
        .unwrap_or_default();
      lend_in(state, id, |lent| {
        step(&Sources {
          index: lent.index,
          repository: lent.repository,
          locks: lent.locks,
          now: lent.now,
          policy: Policy {
            anonymous: slates_svn::authz::Access::None,
            authenticated: slates_svn::authz::Access::Write,
            authz: None,
            filesystem: &name,
          },
          texts,
        })
      })
    })
    .flatten()
  }

  fn commit(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    work: Work,
    texts: &dyn slates_svn::commit::drive::TextSink,
  ) -> Option<Result<Committed, svn_commit::Uncommitted>> {
    crate::state::with_state(|state| {
      let now = AprTime::from_wall_ns(state.clock.wall_ns());
      svn_commit::commit_work(state, (principal, id), work, now, texts)
    })
  }

  fn land_record(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    record: slates_svn::commit::record::Record,
  ) -> Option<Result<(), svn_commit::Uncommitted>> {
    crate::state::with_state(|state| svn_commit::record(state, (principal, id), record))
  }
}
