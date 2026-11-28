//! A Mercurial repository's kept rendered length on its owner shard (goal 93, cut 2; main 01a20182's rulings of
//! 2026-11-27 18:56, 18:58 and 20:19): each revision's chunk length as a stream sends it, kept in the index where the
//! revision lands ([`size_fork`], [`keep_in`]; `slates_hg::push::sized`), so a stream clone names its head and writes
//! its index entries with no count walk before its first byte (hg-src at 343703bd7: the count stage 464,254 steps and
//! about 3.8 s of a 15.4 s stream, main's run 1 of 2026-11-27 19:39).
//!
//! **Where it lands.** A push's fork just before its landing ([`size_fork`] from `hg_push::land`, after any re-base, so
//! the rows are the fork's own), its segments' chunk lengths noted as each lands (`hg_push::segment_landed`) and the
//! rest compressed once there; a made changeset's fork before its changeset is made (`hg_made::prepared`); an import's
//! index before it is placed, its segments' chunk lengths noted as each lands (`hg_import::land`).
//!
//! **After a rebuild** ([`rebuilt`], beside `index_upkeep::built` at a replay's end). A replay appends each record's rows
//! as the record names them (`slates_hg::import::record::replay`, a push's and an import's alike), and no record names a
//! chunk length (main's ruling (C): the import record stays as it is), so a rebuilt index is unsized and serves no
//! stream (`1\n`, the client clones) until its recount ends: the count walk once, `slates_hg::render::Recount`, a step
//! budget a slice on a task of its own, its asks answered by the node's changeset store as a stream's are. The trade-off
//! (main's ruling of 20:19): sizing inline in the replay would lengthen the recovery every client of the repository
//! waits on, while an unsized repository answers only stream clones `1\n`, for about one count walk's time (hg-src: the
//! 3.8 s above), and they fall back to a normal clone. A fork swapped in meanwhile (a push landed: a new generation)
//! starts the recount again, its sized rows kept as they are, so what the swap dropped is sized again; one recount a
//! repository, so bounded by the partition's repositories. Counted: its runs, slices, restarts, ends and short ends
//! (non-vacuity).

use std::collections::BTreeSet;

use slates_db::catalog::RepositoryId;
use slates_hg::Admission;
use slates_hg::index::HgIndex;
use slates_hg::push::sized::{self, Landed};
use slates_hg::render::{Recount, StoreWants};

use super::within_budget;
use crate::repository::hg_push::Forked;
use crate::state::ShardState;

/// Counter: revisions sized where they landed. Format: a counter name in the status report.
pub(crate) const SIZED: &str = "vcs_store.hg_sized.revisions";
/// Counter: logs a landing left unsized, an earlier revision of each not sized (a rebuilt repository before its
/// recount). Format: a counter name.
pub(crate) const UNSIZED_LOGS: &str = "vcs_store.hg_sized.unsized_logs";
/// Counter: a landing whose revisions could not be sized (a place past its payload, the compressor's refusal, the
/// index's): its repository serves no stream until a recount. Format: a counter name.
pub(crate) const SIZED_REFUSED: &str = "vcs_store.hg_sized.refused";
/// Counter: recounts begun. Format: a counter name.
pub(crate) const RECOUNTS: &str = "vcs_store.hg_recount.runs";
/// Counter: a recount's slices. Format: a counter name.
pub(crate) const RECOUNT_SLICES: &str = "vcs_store.hg_recount.slices";
/// Counter: a recount begun again, its repository's index swapped meanwhile. Format: a counter name.
pub(crate) const RECOUNT_RESTARTS: &str = "vcs_store.hg_recount.restarts";
/// Counter: a recount ended with every log sized. Format: a counter name.
pub(crate) const RECOUNT_DONE: &str = "vcs_store.hg_recount.done";
/// Counter: a recount ended short (a revision the store does not keep, a changelog no stream can send, the store's or
/// the index's refusal): its repository serves no stream. Format: a counter name.
pub(crate) const RECOUNT_SHORT: &str = "vcs_store.hg_recount.short";

/// The repositories whose recount runs, one each.
#[derive(Debug, Default)]
pub(crate) struct Recounts(BTreeSet<RepositoryId>);

impl Recounts {
  /// Whether repository `id`'s recount runs.
  #[cfg(test)]
  pub(crate) fn holds(&self, id: RepositoryId) -> bool {
    self.0.contains(&id)
  }
}

/// Sizes `forked`'s kept revisions in its fork where they land (module docs): its segments' lengths noted as they
/// landed, the rest compressed once from its changegroup (a push the size of one segment at most: one step's work).
pub(crate) fn size_fork(state: &mut ShardState, forked: &mut Forked) {
  let mut landed = core::mem::take(&mut forked.sized);
  match sized::raw(&forked.intaken.texts, &forked.changegroup.1) {
    Ok(rest) => landed.extend(rest),
    Err(_) => {
      state.count(SIZED_REFUSED, 1);
      return;
    }
  }
  keep_in(state, &mut forked.fork, &landed);
}

/// Keeps `landed` sized in `index`, within the room their rows take ([`HgIndex::sized_charge`]); counted.
pub(crate) fn keep_in(state: &mut ShardState, index: &mut HgIndex, landed: &[Landed]) {
  if landed.is_empty() {
    return;
  }
  let logs: BTreeSet<&[u8]> = landed
    .iter()
    .map(|revision| revision.path.as_slice())
    .collect();
  let path_bytes = logs
    .iter()
    .map(|path| to_u64(path.len()))
    .fold(0, u64::saturating_add);
  let room = HgIndex::sized_charge(to_u64(landed.len()), (to_u64(logs.len()), path_bytes));
  let kept = within_budget(state, room, |stores| {
    stores
      .content_mut()
      .map(|content| sized::keep(index, content, landed))
  });
  match kept {
    Some(Ok(kept)) => {
      state.count(SIZED, kept.sized);
      state.count(UNSIZED_LOGS, kept.unsized_logs);
    }
    Some(Err(_)) | None => state.count(SIZED_REFUSED, 1),
  }
}

/// `count` as a `u64`, saturating.
fn to_u64(count: usize) -> u64 {
  u64::try_from(count).unwrap_or(u64::MAX)
}

/// Repository `id`'s index was just rebuilt (module docs, "After a rebuild"): its recount begun on a task of its own,
/// unless one runs.
pub(crate) fn rebuilt(state: &mut ShardState, id: RepositoryId) {
  if state.vcs_stores.mercurial_ref(id).is_none() || !state.vcs_stores.hg_recounts.0.insert(id) {
    return;
  }
  state.count(RECOUNTS, 1);
  let begun = slates_rt::futures::spawn(recount(id)).and_then(slates_rt::futures::detach);
  if begun.is_err() {
    state.vcs_stores.hg_recounts.0.remove(&id);
    state.count(RECOUNT_SHORT, 1);
  }
}

/// Where a recount's slice left it.
enum Sliced {
  /// More to count, or the store to ask.
  More,
  /// The index was swapped: begin again.
  Again,
  /// Ended: every log sized, or short.
  Ended,
}

/// Repository `id`'s recount (module docs): from the first log, again where the index was swapped; its place in the
/// running recounts given back at its end.
async fn recount(id: RepositoryId) {
  let disk = crate::state::with_state(|state| state.vcs_disk.clone()).flatten();
  if let Some(disk) = disk.as_ref() {
    while recounted_once(disk, id).await {}
  }
  crate::state::with_state(|state| state.vcs_stores.hg_recounts.0.remove(&id));
}

/// One recount of repository `id` from its first log: slices on the shard, the store's answers between them. Whether to
/// begin again (the index was swapped).
async fn recounted_once(disk: &slates_store::owner::StoreHandle, id: RepositoryId) -> bool {
  let Some(Some(generation)) = crate::state::with_state(|state| state.vcs_stores.generation_of(id))
  else {
    return false;
  };
  let mut walk = Recount::new();
  loop {
    match crate::state::with_state(|state| slice(state, id, generation, &mut walk)) {
      Some(Sliced::More) => {}
      Some(Sliced::Again) => return true,
      Some(Sliced::Ended) | None => return false,
    }
    if !answered(disk, id, &mut walk).await {
      return false;
    }
    slates_rt::futures::yield_now().await;
  }
}

/// `walk`'s want, if any, answered from `disk` as a stream's are, the owner copying at most a step's bytes a request:
/// whether it was (a store that cannot answer ends the recount short, counted).
async fn answered(
  disk: &slates_store::owner::StoreHandle,
  id: RepositoryId,
  walk: &mut Recount,
) -> bool {
  if walk.wants().is_none() {
    return true;
  }
  let step =
    crate::state::with_state(|state| super::hg_view::text_bytes_a_step(state)).unwrap_or(1);
  let repository = u128::from_be_bytes(id.bytes);
  if super::hg_view::answer_wants_over(disk, repository, walk, step)
    .await
    .is_some()
  {
    return true;
  }
  crate::state::with_state(|state| state.count(RECOUNT_SHORT, 1));
  false
}

/// One slice of repository `id`'s recount over the index of `generation`: steps until the shard's step budget is spent
/// (at least one) or the store is asked, each log counted kept before the next step.
fn slice(state: &mut ShardState, id: RepositoryId, generation: u64, walk: &mut Recount) -> Sliced {
  state.count(RECOUNT_SLICES, 1);
  if state.vcs_stores.generation_of(id) != Some(generation) {
    state.count(RECOUNT_RESTARTS, 1);
    return Sliced::Again;
  }
  let budget = slates_rt::futures::step_budget_ns().unwrap_or(0);
  let began = slates_rt::futures::now_ns();
  let copy = super::hg_view::step_copy(state);
  walk.set_step_copy(copy);
  let step_bytes = copy.bytes;
  loop {
    let stepped = state
      .vcs_stores
      .mercurial(id)
      .map(|(content, store)| walk.step(&store.index, content, &mut Admission::new(step_bytes)));
    let room = walk.counted_charge();
    let kept = within_budget(state, room, |stores| {
      stores
        .mercurial(id)
        .map(|(content, store)| walk.keep_counted(&mut store.index, content))
    });
    match (stepped, kept) {
      (Some(Ok(true)), Some(Ok(_))) => {}
      (Some(Ok(false)), Some(Ok(_))) => {
        state.count(
          if walk.ended_short() {
            RECOUNT_SHORT
          } else {
            RECOUNT_DONE
          },
          1,
        );
        return Sliced::Ended;
      }
      _ => {
        state.count(RECOUNT_SHORT, 1);
        return Sliced::Ended;
      }
    }
    if walk.wants().is_some() || slates_rt::futures::now_ns().saturating_sub(began) >= budget {
      return Sliced::More;
    }
  }
}
