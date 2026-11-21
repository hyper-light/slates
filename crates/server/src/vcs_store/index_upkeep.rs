//! The shared index's upkeep on its owner shard (goal 93 perf; main's ruling of 2026-11-21 01:05: wire the index's
//! merge once, where the index is owned): a repository's `slates_vcs::Index` merged a table at a time, so a lookup reads
//! a few runs, not one a seal (`slates_vcs::index`, "Merging"; the learned rule, `slates_vcs::rule`).
//!
//! **Why.** A lookup reads the tail, then the runs from the newest until one holds the key, and nothing merged them: an
//! import of Mercurial's own history left 155 runs, a lookup visiting 123 at p50 (11.8 µs), 2 after one merge (0.6 µs;
//! `crates/hg/examples/index_runs.rs`), and every request of a fetch makes hundreds of lookups.
//!
//! **When.** Two ways, one merge:
//! - An index built whole ([`built`]: an import placed, a replay or a rebuild ended) has every run it alone holds merged:
//!   it was written by its seals and read by no one yet, and every request after it reads each run it leaves, so the
//!   learned rule's rent is certain before any lookup is timed.
//! - Then each lend over the store's own index reports what its lookups took ([`looked_up`]: `Index::lookups_took`), and
//!   a merge the rule says is due (`Index::compaction_due`) begins: the rule's rent-or-buy over the deployment's own
//!   timings decides which newest runs, and when. An index never merged has no price (a merge's measured time) for the
//!   rule to weigh, so its first reported lookups merge it once, which times one (a repository filled by pushes alone).
//! A fetch reads a fork of the index (A-745's generation), whose runs are shared, so it neither reports nor merges: runs a
//! fork shares are never merged, and a merge whose runs a fork comes to share is given up (`MergeStep::GivenUp`).
//!
//! **Sliced** (bounded work, CLAUDE.md §3). A merge is made a table at a time (`Index::merge_step`), each slice taking
//! steps until the shard's step budget is spent (at least one), by a task of its own; nothing changes until its last
//! step, so lookups read the runs it merges meanwhile. Its time across its slices is reported as the merge's
//! (`Index::compaction_took`), the rule's price. One merge a repository at a time, kept by the repository's key, so the
//! merges are bounded by the partition's repositories. Each step is lent every byte the shard could admit now, as an
//! import's turn is: the merged run's tables are written before the merged runs give theirs back.
//!
//! **Which indexes.** Every tool's that is a `slates_vcs::Index`: Mercurial's (`HgIndex::rows`) and git's repository
//! index (`slates_git::store::Repository::rows`), each reported from its lend over the store's own index
//! (`hg_view::lend_in`, `upload::lend_on`). Subversion's records keep their own index (`slates_svn::index::Index`), which
//! is not this one.

use std::collections::BTreeMap;

use slates_db::catalog::RepositoryId;
use slates_vcs::{ContentStore, Index, Merge, MergeStep};

use super::{RepositoryStore, VcsStores, within_budget};
use crate::state::ShardState;

/// Counter: an index merge ended, its runs one. Format: a counter name in the status report.
pub(crate) const MERGED: &str = "vcs_store.index_upkeep.merged";
/// Counter: a merge step (a table cut, or the run admitted); more steps than merges shows the merges were sliced.
/// Format: a counter name.
pub(crate) const STEPS: &str = "vcs_store.index_upkeep.steps";
/// Counter: a merge given up (a fork came to share its runs) or refused by the store (past its room); nothing changed.
/// Format: a counter name.
pub(crate) const GIVEN_UP: &str = "vcs_store.index_upkeep.given_up";

/// The merges under way, by the repository's key.
#[derive(Debug, Default)]
pub(crate) struct Upkeep(BTreeMap<RepositoryId, Job>);

/// One repository's merge under way, and the time its steps took so far.
#[derive(Debug)]
struct Job {
  merge: Merge,
  spent_ns: u64,
}

/// Repository `id`'s shared index and the content store, where its store has one this upkeep serves.
fn index_of(stores: &mut VcsStores, id: RepositoryId) -> Option<(&mut ContentStore, &mut Index)> {
  let content = stores.content.as_mut()?;
  match stores.entries.get_mut(&id)? {
    RepositoryStore::Mercurial(store) => Some((content, store.index.rows())),
    RepositoryStore::Git(store) => Some((content, store.repository.rows())),
    RepositoryStore::Subversion(_) => None,
  }
}

/// Repository `id`'s index was just built whole (module docs): every run it alone holds merged, a slice at a time.
pub(crate) fn built(state: &mut ShardState, id: RepositoryId) {
  begin(state, id, usize::MAX);
}

/// A lend over repository `id`'s own index took `elapsed_ns`, its lookups among it (module docs): reported to the rule,
/// and the merge it says is due begun.
pub(crate) fn looked_up(state: &mut ShardState, id: RepositoryId, elapsed_ns: u64) {
  if state.vcs_stores.index_upkeep.0.contains_key(&id) {
    return;
  }
  let due = {
    let Some((content, index)) = index_of(&mut state.vcs_stores, id) else {
      return;
    };
    index.lookups_took(elapsed_ns);
    // The rule's price is a merge's measured time (or inserts' reported time), so an index never merged has none and
    // no merge would ever come due: its first lookups merge it once, which times the price, and the rule decides every
    // merge after (a repository filled by pushes alone is never built whole until a restart replays it).
    index
      .compaction_due(content)
      .or_else(|| (index.counters().compactions == 0).then_some(usize::MAX))
  };
  if let Some(runs) = due {
    begin(state, id, runs);
  }
}

/// Begins a merge of repository `id`'s newest `runs` own runs, its task started; nothing where one is under way or there
/// are fewer than two such runs.
fn begin(state: &mut ShardState, id: RepositoryId, runs: usize) {
  if state.vcs_stores.index_upkeep.0.contains_key(&id) {
    return;
  }
  let Some(merge) = index_of(&mut state.vcs_stores, id)
    .and_then(|(content, index)| index.merge_begin(content, runs))
  else {
    return;
  };
  state
    .vcs_stores
    .index_upkeep
    .0
    .insert(id, Job { merge, spent_ns: 0 });
  if let Ok(task) = slates_rt::futures::spawn(drive(id)) {
    let _ = slates_rt::futures::detach(task);
  }
}

/// Drives repository `id`'s merge a slice at a time, yielding between slices, until it ends.
async fn drive(id: RepositoryId) {
  while crate::state::with_state_counted(|state| {
    let budget = slates_rt::futures::step_budget_ns().unwrap_or(0);
    slice(state, id, budget)
  }) == Some(true)
  {
    slates_rt::futures::yield_now().await;
  }
}

/// One slice of repository `id`'s merge: steps until `budget_ns` is spent, at least one. Whether more follow.
pub(crate) fn slice(state: &mut ShardState, id: RepositoryId, budget_ns: u64) -> bool {
  let began = slates_rt::futures::now_ns();
  loop {
    let Some(mut job) = state.vcs_stores.index_upkeep.0.remove(&id) else {
      return false;
    };
    let started = slates_rt::futures::now_ns();
    let room = state.store.admittable();
    let stepped = within_budget(state, room, |stores| {
      let (content, index) = index_of(stores, id)?;
      Some(index.merge_step(content, &mut job.merge))
    });
    let now = slates_rt::futures::now_ns();
    job.spent_ns = job.spent_ns.saturating_add(now.saturating_sub(started));
    state.count(STEPS, 1);
    match stepped {
      Some(Ok(MergeStep::More)) => {
        state.vcs_stores.index_upkeep.0.insert(id, job);
        if now.saturating_sub(began) >= budget_ns {
          return true;
        }
      }
      Some(Ok(MergeStep::Done(_))) => {
        if let Some((_, index)) = index_of(&mut state.vcs_stores, id) {
          index.compaction_took(job.spent_ns);
        }
        state.count(MERGED, 1);
        return false;
      }
      Some(Ok(MergeStep::GivenUp) | Err(_)) | None => {
        state.count(GIVEN_UP, 1);
        return false;
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_ipc::protocol::{ObjectFormat as WireFormat, ReplyBody, RequestBody, VcsTool};

  /// Goal 93 perf (main's ruling of 2026-11-21 01:05: the merge wired once, for every tool whose index is
  /// `slates_vcs::Index`). Do: create a repository of `tool` on a store-backed shard, give its index four runs of its own
  /// (rows sealed one batch at a time), mark the index built, and drive the upkeep's slices with no budget (one step
  /// each). Expect: one merge (the non-vacuity counter), made in more steps than one, one run left, every row read back,
  /// each lookup visiting at most the tail and that run.
  fn an_index_built_whole_is_merged_into_one_run(tool: VcsTool, format: WireFormat) {
    crate::test_store::audit_on_stored_shard("upkeep", move |state| {
      let owner = slates_db::catalog::Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1234,
      };
      let reply = crate::test_store::create(
        state,
        &owner,
        RequestBody::CreateRepository {
          name: "upkeep".to_owned(),
          tool,
          format,
          default_branch: "branch:default:0".to_owned(),
          green: None,
        },
      );
      assert!(matches!(reply, ReplyBody::GreenCreated { .. }), "{reply:?}");
      let id = state
        .db
        .partition()
        .repositories()
        .repository_named("upkeep")
        .unwrap()
        .id;
      let keys: Vec<Vec<u8>> = (0..4u8)
        .flat_map(|batch| (0..64u8).map(move |at| vec![b'Z', batch, at]))
        .collect();
      let room = state.store.admittable();
      let runs = within_budget(state, room, |stores| {
        let (content, index) = index_of(stores, id).unwrap();
        let before = index.run_tables().len();
        for batch in keys.chunks(64) {
          for key in batch {
            index
              .insert(
                content,
                slates_vcs::NewRow {
                  key,
                  contents: &[],
                  payload: &[7; 512],
                },
              )
              .unwrap();
          }
          index.seal_all(content).unwrap();
        }
        index.run_tables().len() - before
      });
      assert_eq!(runs, 4);
      built(state, id);
      let mut slices = 0usize;
      while slice(state, id, 0) {
        slices = slices.saturating_add(1);
        assert!(slices < 10_000, "the merge ended");
      }
      let counted =
        |state: &ShardState, name: &'static str| state.refusals.get(name).copied().unwrap_or(0);
      assert_eq!((counted(state, MERGED), counted(state, GIVEN_UP)), (1, 0));
      assert!(
        counted(state, STEPS) > 1,
        "sliced: {}",
        counted(state, STEPS)
      );
      let (content, index) = index_of(&mut state.vcs_stores, id).unwrap();
      assert_eq!(index.run_tables().len(), 1);
      for key in &keys {
        let before = index.visits();
        assert!(index.get(content, key).is_some(), "{key:?}");
        assert!(index.visits().saturating_sub(before) <= 2);
      }
    });
  }

  /// An index never merged (a repository filled by pushes alone) is merged once its lookups are reported, which gives
  /// the rule its price. Do: give a Mercurial repository's index four runs of its own; report one lend's lookups; drive
  /// the slices. Expect: one merge, one run left.
  #[test]
  fn an_index_never_merged_is_merged_once_its_lookups_are_reported() {
    crate::test_store::audit_on_stored_shard("upkeep-push", |state| {
      let owner = slates_db::catalog::Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1234,
      };
      let reply = crate::test_store::create(
        state,
        &owner,
        RequestBody::CreateRepository {
          name: "pushed".to_owned(),
          tool: VcsTool::Mercurial,
          format: WireFormat::Sha1,
          default_branch: "branch:default:0".to_owned(),
          green: None,
        },
      );
      assert!(matches!(reply, ReplyBody::GreenCreated { .. }), "{reply:?}");
      let id = state
        .db
        .partition()
        .repositories()
        .repository_named("pushed")
        .unwrap()
        .id;
      let room = state.store.admittable();
      within_budget(state, room, |stores| {
        let (content, index) = index_of(stores, id).unwrap();
        for batch in 0..4u8 {
          for at in 0..64u8 {
            index
              .insert(
                content,
                slates_vcs::NewRow {
                  key: &[b'Z', batch, at],
                  contents: &[],
                  payload: &[7; 512],
                },
              )
              .unwrap();
          }
          index.seal_all(content).unwrap();
        }
      });
      looked_up(state, id, 1_000);
      while slice(state, id, 0) {}
      let merged = state.refusals.get(MERGED).copied().unwrap_or(0);
      let (_, index) = index_of(&mut state.vcs_stores, id).unwrap();
      assert_eq!((merged, index.run_tables().len()), (1, 1));
    });
  }

  /// [`an_index_built_whole_is_merged_into_one_run`] for Mercurial's index.
  #[test]
  fn a_mercurial_index_built_whole_is_merged_a_table_a_step_into_one_run() {
    an_index_built_whole_is_merged_into_one_run(VcsTool::Mercurial, WireFormat::Sha1);
  }

  /// [`an_index_built_whole_is_merged_into_one_run`] for git's repository index.
  #[test]
  fn a_git_index_built_whole_is_merged_a_table_a_step_into_one_run() {
    an_index_built_whole_is_merged_into_one_run(VcsTool::Git, WireFormat::Sha1);
  }
}
