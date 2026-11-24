//! A push's fork re-based onto its repository's index as it is at its entry's commit (A-720 part 2b; main 01a20182's
//! rulings of 2026-11-22 07:32 and 12:00): another landing replaced the index the fork was made from while the push's
//! segments landed apart from the landing turn. Each row the push kept is appended again by node, a slice at a time
//! (`intake::Rebase`), the same rows a restart's replay of its segments' records makes over that index (v10, held by
//! `push::intake::tests::a_stale_forks_re_base_and_its_segments_replay_make_the_same_index`).

use slates_db::catalog::RepositoryId;
use slates_hg::changegroup::Version;
use slates_hg::push::commit::Unindexed;
use slates_hg::push::intake::{Rebase, Stepped};

use super::{Forked, PushOutcome, abort, release_index, unindexed_words};
use crate::state::ShardState;
use crate::vcs_store;

/// A stale fork's re-base while it runs (A-720 part 2b), given back by its drop if it is not finished.
pub(super) struct Rebasing {
  /// The re-base, until it is finished or given back.
  rebase: Option<Rebase>,
  /// The generation of the index it re-bases onto.
  generation: u64,
  /// The push's changegroup as it arrived, carried to the re-based fork; taken by its finish.
  changegroup: Option<(Version, Vec<u8>)>,
  /// The room the push's intake began within.
  room: u64,
  /// The push's record, where written before the re-base.
  record: Option<Vec<u8>>,
  /// Its segments cut and not yet landed, carried as they are: their records name rows by node (v10), whatever index
  /// they land over.
  segments: Vec<super::RecordedSegment>,
}

impl Rebasing {
  /// One slice: rows re-appended until the re-base is done or the shard's quantum is spent; none when the content
  /// store is gone.
  pub(super) fn slice(&mut self, state: &mut ShardState) -> Option<Result<Stepped, Unindexed>> {
    let rebase = self.rebase.as_mut()?;
    let quantum = vcs_store::hg_import::quantum_of(state);
    let start = slates_rt::futures::now_ns();
    vcs_store::within_budget(state, 0, |stores| {
      let content = stores.content_mut()?;
      Some(loop {
        match rebase.step(content) {
          Ok(Stepped::More) if slates_rt::futures::now_ns().saturating_sub(start) < quantum => {}
          other => break other,
        }
      })
    })
  }

  /// The re-based fork, the stale one given back; none, counted, when its release is refused or the store is gone.
  pub(super) fn finish_in(mut self, state: &mut ShardState) -> Option<Forked> {
    let changegroup = self.changegroup.take()?;
    let rebase = self.rebase.take()?;
    let generation = self.generation;
    let finished = vcs_store::within_budget(state, 0, |stores| {
      stores.content_mut().map(|content| rebase.finish(content))
    });
    match finished {
      Some(Ok((fork, intaken))) => Some(Forked {
        fork,
        intaken,
        generation,
        record: self.record.take(),
        changegroup,
        room: self.room,
        segments: core::mem::take(&mut self.segments),
      }),
      _ => {
        state.count(vcs_store::RELEASE_REFUSED, 1);
        None
      }
    }
  }
}

impl Drop for Rebasing {
  fn drop(&mut self) {
    if let Some(rebase) = self.rebase.take() {
      // Dropped mid-re-base (its connection ended): both forks given back on the shard that holds the store.
      crate::state::with_state(|state| {
        let released = vcs_store::within_budget(state, 0, |stores| {
          stores
            .content_mut()
            .map_or(Ok(()), |content| rebase.abandon(content))
        });
        if released.is_err() {
          state.count(vcs_store::RELEASE_REFUSED, 1);
        }
      })
      .unwrap_or_default();
    }
  }
}

/// Starts re-basing `stale` onto `repository`'s index as it is now; none when its store is not held here.
pub(super) fn begin_rebase(
  state: &mut ShardState,
  repository: RepositoryId,
  stale: Forked,
) -> Option<Result<Rebasing, PushOutcome>> {
  let Forked {
    fork,
    intaken,
    changegroup,
    room,
    record,
    segments,
    ..
  } = stale;
  // A refusal hands the fork back, boxed: an index is far larger than the rebase begun.
  let begun = vcs_store::within_budget(state, 0, |stores| {
    let Some(generation) = stores
      .generation_of(repository)
      .filter(|_| !stores.rebuilding(repository))
    else {
      return Err(Box::new((None, fork)));
    };
    let Some((content, store)) = stores.mercurial(repository) else {
      return Err(Box::new((None, fork)));
    };
    Rebase::begin(fork, &intaken, &mut store.index, content)
      .map(|rebase| Rebasing {
        rebase: Some(rebase),
        generation,
        changegroup: Some(changegroup),
        room,
        record,
        segments,
      })
      .map_err(|handed| {
        let (refused, fork) = *handed;
        Box::new((Some(refused), fork))
      })
  });
  match begun {
    Ok(rebasing) => Some(Ok(rebasing)),
    Err(refused) => {
      let (refused, fork) = *refused;
      if release_index(state, fork).is_err() {
        state.count(vcs_store::RELEASE_REFUSED, 1);
      }
      refused.map(|refused| Err(PushOutcome::refused(&abort(unindexed_words(&refused)))))
    }
  }
}

/// `stale` re-based a slice at a time, a yield between slices (A-720 part 2b); none when the store went away.
pub(super) async fn rebase_sliced(
  repository: RepositoryId,
  stale: Forked,
) -> Option<Result<Forked, PushOutcome>> {
  // A-740: a shard stuck in this step names it.
  let _doing = slates_rt::futures::doing("hg_push::rebase");
  let begun = crate::state::with_state(|state| begin_rebase(state, repository, stale));
  let mut rebasing = match begun.flatten()? {
    Ok(rebasing) => rebasing,
    Err(outcome) => return Some(Err(outcome)),
  };
  loop {
    let sliced = crate::state::with_state(|state| rebasing.slice(state));
    match sliced.flatten()? {
      Ok(Stepped::More) => slates_rt::futures::yield_now().await,
      Ok(Stepped::Done) => {
        return crate::state::with_state(|state| rebasing.finish_in(state))
          .flatten()
          .map(Ok);
      }
      // A manifest's text not at hand, let go once landed (goal 93, C1 (1)): read from the node's changeset store between
      // slices, as the intake reads one, and the slice taken again.
      Ok(Stepped::Store) => {
        let rebase = rebasing.rebase.as_mut()?;
        if !vcs_store::hg_view::answer_store_wants(rebase, repository).await {
          return Some(Err(PushOutcome::refused(&abort(REBASE_UNREAD.to_vec()))));
        }
      }
      // A re-base reads the fork's rows, never the changegroup's bytes.
      Ok(Stepped::Wanted(_)) => {
        return Some(Err(PushOutcome::refused(&abort(REBASE_WANTED.to_vec()))));
      }
      Err(refused) => return Some(Err(PushOutcome::refused(&abort(unindexed_words(&refused))))),
    }
  }
}

/// Format: slates' words for a re-base whose manifest text, let go once landed, the node's changeset store did not give.
const REBASE_UNREAD: &[u8] =
  b"push refused: its re-base could not read a manifest from the changeset store";

/// Format: slates' words for a re-base that asked for a changegroup's bytes, which none reads.
const REBASE_WANTED: &[u8] = b"push refused: its re-base asked for bytes of its changegroup";

/// `stale` re-based every row at once: [`rebase_sliced`]'s steps for a caller that holds the shard's state, the tests'
/// driver, which lands pushes side by side with no runtime. None where the store went away.
#[cfg(test)]
pub(super) fn rebase_whole(
  state: &mut ShardState,
  repository: RepositoryId,
  stale: Forked,
) -> Option<Result<Forked, PushOutcome>> {
  let mut rebasing = match begin_rebase(state, repository, stale)? {
    Ok(rebasing) => rebasing,
    Err(outcome) => return Some(Err(outcome)),
  };
  loop {
    match rebasing.slice(state)? {
      Ok(Stepped::More) => {}
      Ok(Stepped::Done) => return rebasing.finish_in(state).map(Ok),
      Ok(Stepped::Wanted(_) | Stepped::Store) => {
        return Some(Err(PushOutcome::refused(&abort(REBASE_WANTED.to_vec()))));
      }
      Err(refused) => return Some(Err(PushOutcome::refused(&abort(unindexed_words(&refused))))),
    }
  }
}
